//! The binding contract every transport keeps (`transports/index.mdx`),
//! client side: the HTTP transport over both bindings, a custom in-process
//! transport, and the pipeline judging what either delivers.

mod transport_support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agui_sdk::client::transport::sse::{SseDecoder, DEFAULT_MAX_EVENT_BYTES};
use agui_sdk::client::transport::{
    json_frame, BoxTransport, DynTransport, HttpTransport, Transport, TransportRequest,
};
use agui_sdk::client::{Frame, FrameStream};
use agui_sdk::pipeline::Termination;
use agui_sdk::wire::Chunk;
use agui_sdk::ErrorCode;
use serde_json::{json, Value};
use transport_support::*;

fn http(server: &MockServer) -> HttpTransport {
    HttpTransport::builder(server.url("/run"))
        .build()
        .unwrap_or_else(|e| panic!("{e}"))
}

/// Opens one run against a server answering `reply`.
fn exchange(reply: Reply) -> (Vec<Value>, Option<agui_sdk::Error>, Vec<Captured>) {
    block(async move {
        let server = MockServer::always(reply)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let stream = http(&server)
            .open(request())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let (frames, error) = drain(stream).await;
        (frames, error, server.requests())
    })
}

/// covers(client): TRN-01
#[test]
fn delivery_is_ordered_and_complete_under_any_chunking() {
    let mut script = events();
    for i in 0..40 {
        script.insert(
            4,
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m-r1", "delta": format!("#{i}")}),
        );
    }
    for piece in [1, 2, 7, 64, 4096] {
        let (frames, error, _) = exchange(Reply::sse(sse(&script)).pieces(piece, Duration::ZERO));
        assert!(error.is_none());
        assert_eq!(frames, script, "piece {piece}");
    }
}

/// covers(client): TRN-02
#[test]
fn the_input_is_delivered_whole_before_any_event() {
    // The mock answers only after reading the full request body; the run's
    // events then arrive on the same exchange.
    let (frames, _, requests) = exchange(Reply::sse(sse(&events())));
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].json(), Some(input()));
    assert_eq!(frames, events());
}

/// covers(client): PAT-01
#[test]
fn the_consumer_speaks_once_per_exchange() {
    // One run: exactly one request, carrying the input; nothing is sent
    // while the events stream (a slow stream still sees one request).
    let (frames, error, requests) =
        exchange(Reply::sse(sse(&events())).pieces(16, Duration::from_millis(5)));
    assert!(error.is_none());
    assert_eq!(frames, events());
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
}

#[test]
fn no_mid_run_message_is_needed_to_finish_a_run() {
    // The client has no channel to the producer during a run; a run that
    // pauses (an interrupt) finishes, and outside input goes in the next
    // run's request.
    let script = vec![
        json!({"type": "RUN_STARTED", "threadId": "t1", "runId": "r1"}),
        json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": "r1",
               "outcome": {"type": "interrupt", "interrupts": [{"id": "i1", "reason": "approval"}]}}),
    ];
    block(async move {
        let server = MockServer::always(Reply::sse(sse(&script)))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = http(&server);
        let (frames, error) = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert!(error.is_none());
        assert_eq!(types(&frames), ["RUN_STARTED", "RUN_FINISHED"]);
        assert_eq!(server.requests().len(), 1);
        let _ = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert_eq!(
            server.requests().len(),
            2,
            "the next input is a new request"
        );
    });
}

#[test]
fn replayed_runs_arrive_on_the_one_request() {
    // A response replaying an earlier run ahead of the requested one: both
    // runs, each with its own RUN_STARTED (one echoing its input), on the
    // single request; the client sends nothing more.
    let mut script = run("t1", "r0", "earlier");
    script[0]["input"] = json!({"threadId": "t1", "runId": "r0", "state": {}, "messages": [],
                                "tools": [], "context": [], "forwardedProps": {}});
    script.extend(run("t1", "r1", "now"));
    let (frames, error, requests) = exchange(Reply::sse(sse(&script)));
    assert!(error.is_none());
    assert_eq!(frames, script);
    assert_eq!(requests.len(), 1);
    assert_eq!(judge(&frames).ok(), Some(Termination::Complete));
}

/// covers(client): TRN-04
#[test]
fn termination_is_distinguishable_from_truncation() {
    let body = sse(&events());
    let (complete, error, _) = exchange(Reply::sse(body.clone()));
    assert!(error.is_none());
    assert_eq!(judge(&complete).ok(), Some(Termination::Complete));

    let cut = sse(&events()[..4]).len();
    let (partial, error, _) = exchange(Reply::sse(body).framing(Framing::Length).cut_after(cut));
    assert!(error.is_none());
    assert_eq!(judge(&partial).ok(), Some(Termination::Truncated));
}

/// covers(client): TRN-05
#[test]
fn refusal_is_an_error_from_open_not_frames() {
    for status in [400u16, 401, 403, 422, 500] {
        let result = block(async move {
            let server = MockServer::always(
                Reply::status(status)
                    .header("Content-Type", "application/json")
                    .body(r#"{"error":"invalid input"}"#),
            )
            .await
            .unwrap_or_else(|e| panic!("{e}"));
            http(&server).open(request()).await
        });
        let error = expect_code(result, ErrorCode::HttpStatus);
        assert_eq!(error.http_status(), Some(status));
        assert!(!error.run_started());
        let body = std::error::Error::source(&error).map(|s| s.to_string());
        assert_eq!(body.as_deref(), Some(r#"{"error":"invalid input"}"#));
    }
}

/// covers(client): TRN-07
#[test]
fn http_speaks_the_sse_binding() {
    let (frames, error, requests) = exchange(Reply::sse(sse(&events())));
    assert!(error.is_none());
    assert_eq!(frames, events());
    assert!(requests[0]
        .header("accept")
        .unwrap_or_default()
        .contains("text/event-stream"));
}

/// covers(client): TRN-08
#[cfg(feature = "proto")]
#[test]
fn http_speaks_the_protobuf_binding() {
    for piece in [0, 1, 5] {
        let (frames, error, requests) =
            exchange(Reply::proto(proto(&events())).pieces(piece, Duration::ZERO));
        assert!(error.is_none(), "{error:?}");
        assert_eq!(frames, events(), "piece {piece}");
        assert!(requests[0]
            .header("accept")
            .unwrap_or_default()
            .contains("application/vnd.ag-ui.event+proto"));
    }
    // The same frames as SSE: the bindings differ only in framing.
    let (sse_frames, _, _) = exchange(Reply::sse(sse(&events())));
    let (proto_frames, _, _) = exchange(Reply::proto(proto(&events())));
    assert_eq!(sse_frames, proto_frames);
    assert_eq!(judge(&proto_frames).ok(), Some(Termination::Complete));
}

/// covers(client): TRN-09
#[test]
fn a_truncated_run_gets_no_synthesized_finish() {
    let cut = sse(&events()[..2]).len();
    let (frames, _, _) = exchange(
        Reply::sse(sse(&events()))
            .framing(Framing::Close)
            .cut_after(cut),
    );
    assert_eq!(types(&frames), ["RUN_STARTED", "TEXT_MESSAGE_START"]);
    assert!(!types(&frames).iter().any(|t| t == "RUN_FINISHED"));
    assert_eq!(judge(&frames).ok(), Some(Termination::Truncated));
}

/// covers(client): TRN-10
#[test]
fn a_truncated_run_is_never_success() {
    // Cut inside an event: the stream's last item is an error, and the
    // frames before it judge as truncated, not complete.
    let cut = sse(&events()[..3]).len() + 5;
    let (frames, error, _) = exchange(
        Reply::sse(sse(&events()))
            .framing(Framing::Chunked)
            .cut_after(cut),
    );
    assert_eq!(error.map(|e| e.code()), Some(ErrorCode::Truncated));
    assert_ne!(judge(&frames).ok(), Some(Termination::Complete));
}

/// covers(client): TRN-11
#[test]
fn delivered_events_stay_delivered_and_a_rerun_is_a_new_request() {
    block(async {
        let cut = sse(&events()[..3]).len();
        let server = MockServer::sequence(vec![
            Reply::sse(sse(&events()))
                .framing(Framing::Length)
                .cut_after(cut),
            Reply::sse(sse(&run("t1", "r2", "again"))),
        ])
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let transport = http(&server);
        let (first, _) = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert_eq!(first, events()[..3].to_vec(), "kept, not rolled back");
        assert_eq!(server.requests().len(), 1, "no automatic re-run");
        let mut rerun = request();
        let mut body = input();
        body["runId"] = json!("r2");
        rerun.body = Chunk::from(serde_json::to_vec(&body).unwrap_or_default());
        rerun.run_id = "r2".into();
        let (second, _) = drain(
            transport
                .open(rerun)
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert_eq!(judge(&second).ok(), Some(Termination::Complete));
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].json().map(|v| v["runId"].clone()),
            Some(json!("r2"))
        );
    });
}

/// An in-process transport: a scripted channel that frames one JSON event
/// per frame, as the SSE binding does, and records the input it was given.
#[derive(Clone, Default)]
struct Channel {
    script: Vec<Value>,
    received: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Transport for Channel {
    async fn open(&self, request: TransportRequest) -> agui_sdk::Result<FrameStream> {
        if let Ok(mut log) = self.received.lock() {
            log.push(request.body.to_vec());
        }
        let frames: Vec<agui_sdk::Result<Frame>> = self
            .script
            .iter()
            .map(|e| Ok(json_frame(Chunk::from(e.to_string()))))
            .collect();
        Ok(FrameStream::from_frames(frames))
    }
}

/// covers(client): TRN-12
#[test]
fn ag_ui_runs_over_a_custom_channel() {
    let channel = Channel {
        script: events(),
        ..Channel::default()
    };
    let received = channel.received.clone();
    let boxed: BoxTransport = Arc::new(channel);
    let (frames, error) = block(async move {
        let stream = boxed
            .open(request())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        drain(stream).await
    });
    assert!(error.is_none());
    assert_eq!(frames, events());
    let received = received.lock().map(|r| r.clone()).unwrap_or_default();
    assert_eq!(received.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&received[0]).ok(),
        Some(input())
    );
}

/// covers(client): TRN-13
#[test]
fn a_custom_transport_gets_the_same_verdicts_as_http() {
    // The same scripts, delivered by HTTP and by the custom channel, judge
    // the same: transports yield frames, the shared pipeline decides.
    let truncated = events()[..3].to_vec();
    let mut broken = events();
    broken.remove(3); // RUN_FINISHED with the text message still open
    for script in [events(), truncated, broken] {
        let (over_http, _, _) = exchange(Reply::sse(sse(&script)));
        let channel = Channel {
            script: script.clone(),
            ..Channel::default()
        };
        let (over_channel, _) = block(async move {
            let dynamic: &dyn DynTransport = &channel;
            drain(
                dynamic
                    .open_boxed(request())
                    .await
                    .unwrap_or_else(|e| panic!("{e}")),
            )
            .await
        });
        assert_eq!(over_http, over_channel);
        let a = judge(&over_http).map_err(|e| e.code());
        let b = judge(&over_channel).map_err(|e| e.code());
        assert_eq!(a, b);
    }
}

/// covers(client): TRN-14
#[test]
fn the_transport_contract_is_documented() {
    let docs = include_str!("../src/client/transport/mod.rs");
    for section in ["# Framing", "# Input delivery", "# Termination", "# Errors"] {
        assert!(
            docs.contains(&format!("/// {section}")),
            "Transport rustdoc lacks the {section:?} section"
        );
    }
}

/// covers(client): TRN-15
#[test]
fn a_custom_byte_channel_frames_one_json_event_per_frame() {
    // A byte channel reusing the SSE framing: whatever the chunking, each
    // frame is exactly one event object.
    let bytes = sse(&events());
    for size in [1, 3, 17] {
        let mut decoder = SseDecoder::new(DEFAULT_MAX_EVENT_BYTES);
        let mut frames = Vec::new();
        for piece in bytes.chunks(size) {
            decoder.push(Chunk::copy_from_slice(piece));
            while let Some(event) = decoder.next_event().unwrap_or_else(|e| panic!("{e}")) {
                let value = json_frame(event)
                    .into_value()
                    .unwrap_or_else(|e| panic!("{e}"));
                assert!(value.is_object());
                frames.push(value);
            }
        }
        assert_eq!(frames, events());
    }
}

/// covers(client): PROC-07
#[test]
fn transports_yield_frames_and_an_undecodable_frame_fails_decoding() {
    // The transport passes bytes through without judging them; a frame that
    // is not a JSON object fails at decoding, in the pipeline.
    let body = b"data: {\"type\":\"RUN_STARTED\",\"threadId\":\"t1\",\"runId\":\"r1\"}\n\ndata: not json\n\n".to_vec();
    block(async move {
        let server = MockServer::always(Reply::sse(body))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let mut stream = http(&server)
            .open(request())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let first = stream.next().await;
        assert!(matches!(first, Some(Ok(_))));
        let second = stream
            .next()
            .await
            .unwrap_or_else(|| panic!("the transport dropped the frame"))
            .unwrap_or_else(|e| panic!("the transport judged the frame: {e}"));
        let error = expect_code(second.into_value(), ErrorCode::MalformedEvent);
        assert!(!error.message().is_empty());
        assert!(stream.next().await.is_none());
    });
}

/// The server half: this crate's server, through the axum glue, against this
/// crate's client over a real local TCP socket, once per binding.
#[cfg(all(feature = "server", feature = "axum", feature = "client"))]
#[path = "support_producer/mod.rs"]
mod support_producer;

#[cfg(all(feature = "server", feature = "axum", feature = "client"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod server_half {
    use std::sync::{Arc, Mutex};

    use super::support_producer::{agent, input_json, raw_request, runtime, serve, sse_events};
    use agui_sdk::client::{AgentClient, RunRequest, RunStatus};
    use agui_sdk::ids::RunId;
    use agui_sdk::pipeline::{PipelineConfig, Processor};
    use agui_sdk::server::encode::json::NdjsonEncoder;
    use agui_sdk::server::encode::Encoder;
    use agui_sdk::server::http::{handle, HttpRequest, ServeConfig};
    use agui_sdk::server::input::{admit, InputPolicy};
    use agui_sdk::server::{
        CancelToken, Extensions, ReplayedRun, RunOutcome, Runner, RunnerConfig,
    };
    use agui_sdk::tool::Tool;
    use agui_sdk::wire::WireFormat;
    use agui_sdk::{ErrorCode, Event, EventType};
    use serde_json::{json, Value};

    type Log = Arc<Mutex<Vec<EventType>>>;

    /// An agent that answers with a fixed mix of families and logs what it
    /// emitted, so the receiving side can be compared with the sending side.
    fn scripted(log: Log) -> Runner<impl agui_sdk::server::Agent<State = Value>> {
        Runner::new(agent(move |ctx| {
            let log = Arc::clone(&log);
            async move {
                let heard = ctx
                    .messages()
                    .last()
                    .and_then(|m| m.text())
                    .unwrap_or("")
                    .to_owned();
                let e = ctx.emitter();
                let step = e.start_step("answer").await?;
                e.say(format!("you said: {heard}")).await?;
                let call = e.start_tool_call("lookup", None).await?;
                call.push_args("{\"q\":\"x\"}").await?;
                let id = call.end().await?;
                e.tool_result(id, "42").await?;
                e.custom("vendor:done", json!({"ok": true})).await?;
                step.finish().await?;
                log.lock().unwrap().extend([
                    EventType::StepStarted,
                    EventType::TextMessageStart,
                    EventType::TextMessageContent,
                    EventType::TextMessageEnd,
                    EventType::ToolCallStart,
                    EventType::ToolCallArgs,
                    EventType::ToolCallEnd,
                    EventType::ToolCallResult,
                    EventType::Custom,
                    EventType::StepFinished,
                ]);
                Ok(RunOutcome::success())
            }
        }))
    }

    fn config(protobuf: bool) -> ServeConfig {
        let mut config = ServeConfig::default();
        config.protobuf = protobuf;
        config
    }

    /// Runs the scripted agent end to end and returns the client's report and
    /// the event types the client delivered, in order.
    fn end_to_end(protobuf: bool) -> (agui_sdk::client::RunReport, Vec<EventType>, Vec<EventType>) {
        let log: Log = Arc::default();
        let runner = scripted(Arc::clone(&log));
        runtime().block_on(async move {
            let addr = serve(runner, config(protobuf)).await;
            let client = AgentClient::http(format!("http://{addr}/agent"))
                .build()
                .unwrap();
            let mut handle = client.run(RunRequest::new().user("ping"));
            let mut seen = Vec::new();
            while let Some(update) = handle.next().await {
                seen.push(update.unwrap().event.event_type());
            }
            let report = client.run(RunRequest::new().user("ping")).await.unwrap();
            let emitted = log.lock().unwrap().clone();
            (report, seen, emitted)
        })
    }

    fn content_type_for(protobuf: bool) -> String {
        let runner = scripted(Arc::default());
        runtime().block_on(async move {
            let addr = serve(runner, config(protobuf)).await;
            let accept = WireFormat::client_accept(cfg!(feature = "proto"));
            let response = raw_request(
                addr,
                "POST",
                "/agent",
                &[("Content-Type", "application/json"), ("Accept", accept)],
                &input_json("t1", "r1"),
            )
            .await;
            response
                .header("content-type")
                .unwrap_or_default()
                .to_owned()
        })
    }

    /// covers(server): TRN-01, TRN-02
    #[test]
    fn events_arrive_complete_and_in_emission_order_after_the_input() {
        for protobuf in [false, cfg!(feature = "proto")] {
            let (report, seen, emitted) = end_to_end(protobuf);
            // The input reached the agent whole before it emitted (it echoes it).
            let reply = report
                .messages
                .iter()
                .find_map(|m| m.text().filter(|t| t.starts_with("you said")));
            assert_eq!(reply, Some("you said: ping"));
            // Everything emitted arrived, in order, bracketed by the lifecycle.
            assert_eq!(seen.first(), Some(&EventType::RunStarted));
            assert_eq!(seen.last(), Some(&EventType::RunFinished));
            assert_eq!(&seen[1..seen.len() - 1], &emitted[..emitted.len() / 2]);
        }
    }

    /// covers(server): TRN-03
    #[test]
    fn replayed_runs_get_no_further_input() {
        let mut runner_config = RunnerConfig::default();
        runner_config.echo_input = true;
        let runner = scripted(Arc::default()).config(runner_config);
        let admitted = admit(&input_json("t1", "r-now"), &InputPolicy::default()).unwrap();
        let replay = vec![ReplayedRun::new(
            RunId::new("r-before"),
            Vec::new(),
            json!({}),
        )];
        let mut stream = runner
            .run_with_replay(replay, admitted, CancelToken::new(), Extensions::new())
            .unwrap();
        let starts = runtime().block_on(async move {
            let mut starts = Vec::new();
            while let Some(event) = stream.next().await {
                if let Event::RunStarted(s) = event {
                    starts.push((s.run_id.as_str().to_owned(), s.input.is_some()));
                }
            }
            starts
        });
        // The replayed run restates history and takes no input; the requested
        // run may echo the one input it received.
        assert_eq!(
            starts,
            [("r-before".to_owned(), false), ("r-now".to_owned(), true)]
        );
    }

    /// covers(server): TRN-04
    #[test]
    fn the_terminal_event_tells_completion_from_truncation() {
        for protobuf in [false, cfg!(feature = "proto")] {
            let (report, seen, _) = end_to_end(protobuf);
            assert!(matches!(report.status, RunStatus::Finished { .. }));
            assert_eq!(
                seen.iter()
                    .filter(|t| **t == EventType::RunFinished)
                    .count(),
                1
            );
        }
        // The body ends right after the terminal: no byte follows it.
        let runner = scripted(Arc::default());
        let body = input_json("t1", "r1");
        let response = handle(
            &runner,
            &config(false),
            HttpRequest::new(Some("application/json"), None, &body),
            CancelToken::new(),
        )
        .unwrap();
        let mut chunks = response.body;
        let bytes = runtime().block_on(async move {
            let mut all = Vec::new();
            while let Some(chunk) = chunks.next().await {
                all.extend_from_slice(&chunk);
            }
            all
        });
        let events = sse_events(&bytes);
        assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
        assert!(String::from_utf8(bytes).unwrap().ends_with("}\n\n"));
    }

    /// covers(server): TRN-05
    #[test]
    fn invalid_input_is_refused_before_run_started_outside_the_stream() {
        let runner = scripted(Arc::default());
        let refused = handle(
            &runner,
            &config(false),
            HttpRequest::new(Some("application/json"), None, b"[1, 2]"),
            CancelToken::new(),
        )
        .unwrap_err();
        assert_eq!(refused.status, 400);
        assert!(!refused.body.contains("RUN_"));
        // Over the wire the client sees an HTTP error, never a RUN_ERROR.
        let runner = scripted(Arc::default());
        runtime().block_on(async move {
            let mut serve_config = config(false);
            serve_config.input.max_body_bytes = 64;
            let addr = serve(runner, serve_config).await;
            let client = AgentClient::http(format!("http://{addr}/agent"))
                .build()
                .unwrap();
            let error = client
                .run(RunRequest::new().user("x".repeat(500)))
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::HttpStatus);
            assert_eq!(error.http_status(), Some(413));
            assert!(!error.run_started());
        });
    }

    /// covers(server): TRN-07
    #[test]
    fn the_sse_binding_runs_end_to_end() {
        assert_eq!(content_type_for(false), "text/event-stream");
        let (report, seen, _) = end_to_end(false);
        assert!(report.status.is_success());
        assert!(seen.contains(&EventType::ToolCallResult));
    }

    /// covers(server): TRN-08
    #[cfg(feature = "proto")]
    #[test]
    fn the_protobuf_binding_runs_end_to_end() {
        assert_eq!(content_type_for(true), "application/vnd.ag-ui.event+proto");
        let (proto_report, proto_seen, _) = end_to_end(true);
        let (sse_report, sse_seen, _) = end_to_end(false);
        assert!(proto_report.status.is_success());
        // Same events and the same history over either binding (ARCH-06).
        assert_eq!(proto_seen, sse_seen);
        let texts = |r: &agui_sdk::client::RunReport| -> Vec<String> {
            r.messages
                .iter()
                .filter_map(|m| m.text().map(str::to_owned))
                .collect()
        };
        assert_eq!(texts(&proto_report), texts(&sse_report));
    }

    /// NDJSON frames of one run, as a custom channel would carry them.
    fn ndjson_run() -> (Vec<u8>, Vec<Event>) {
        let runner = scripted(Arc::default());
        let admitted = admit(&input_json("t1", "r1"), &InputPolicy::default()).unwrap();
        let mut stream = runner
            .run(admitted, CancelToken::new(), Extensions::new())
            .unwrap();
        runtime().block_on(async move {
            let mut encoder = NdjsonEncoder::new();
            let mut frames = Vec::new();
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                encoder.encode(&event, &mut frames).unwrap();
                events.push(event);
            }
            (frames, events)
        })
    }

    /// covers(server): TRN-12, TRN-13
    #[test]
    fn a_custom_channel_keeps_the_event_model_and_processing_rules() {
        let (frames, sent) = ndjson_run();
        // Carried over any byte channel, the frames go through the same
        // consumer pipeline and are judged by the same rules.
        let mut processor = Processor::new(PipelineConfig::default());
        let sink = Arc::new(agui_sdk::diagnostics::CollectingSink::new());
        let mut reporter = agui_sdk::diagnostics::Reporter::new(sink, 16);
        let mut out = Vec::new();
        for line in frames.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            processor.push_json(line, &mut reporter, &mut out).unwrap();
        }
        let received: Vec<Event> = out.iter().map(|p| (*p.event).clone()).collect();
        assert_eq!(received, sent);
    }

    /// covers(server): TRN-14
    #[test]
    fn the_binding_contract_is_documented_for_custom_transports() {
        let docs = include_str!("../src/server/http.rs");
        for section in [
            "**Input delivery.**",
            "**Negotiation.**",
            "**Admission errors**",
            "**Framing.**",
            "**Ordering and termination.**",
            "**Disconnect.**",
        ] {
            assert!(docs.contains(section), "missing {section}");
        }
    }

    /// covers(server): TRN-15
    #[test]
    fn a_custom_json_channel_frames_one_event_object_per_frame() {
        let (frames, sent) = ndjson_run();
        let lines: Vec<&[u8]> = frames
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), sent.len());
        for (line, event) in lines.iter().zip(&sent) {
            let parsed: Value = serde_json::from_slice(line).unwrap();
            assert!(parsed.is_object());
            assert_eq!(parsed, serde_json::to_value(event).unwrap());
        }
        // The same JSON as the SSE payload.
        let mut sse = Vec::new();
        let mut encoder = Encoder::for_format(WireFormat::Sse);
        for event in &sent {
            encoder.encode(event, &mut sse).unwrap();
        }
        let sse_lines: Vec<&str> = std::str::from_utf8(&sse)
            .unwrap()
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .collect();
        let ndjson_lines: Vec<&str> = lines
            .iter()
            .map(|l| std::str::from_utf8(l).unwrap())
            .collect();
        assert_eq!(sse_lines, ndjson_lines);
    }

    /// covers(server): PAT-01
    #[test]
    fn the_consumer_speaks_once_and_events_flow_one_way() {
        // One request body per exchange; the response is the only channel back.
        let (report, seen, _) = end_to_end(false);
        assert!(report.status.is_success());
        assert_eq!(
            seen.iter().filter(|t| **t == EventType::RunStarted).count(),
            1
        );
    }

    /// covers(server): PAT-02
    #[test]
    fn a_run_needing_outside_input_ends_instead_of_waiting() {
        // A frontend tool call is left to the next run: the run finishes at once,
        // naming the call, instead of waiting for a mid-run answer.
        let runner = Runner::new(agent(|ctx| async move {
            ctx.emitter()
                .start_tool_call("confirm", None)
                .await?
                .end()
                .await?;
            Ok(RunOutcome::success())
        }));
        runtime().block_on(async move {
            let addr = serve(runner, config(false)).await;
            let client = AgentClient::http(format!("http://{addr}/agent"))
                .build()
                .unwrap();
            let report = client
                .run(
                    RunRequest::new()
                        .user("go")
                        .tool(Tool::new("confirm", "ask the user")),
                )
                .await
                .unwrap();
            assert!(report.status.is_success());
            assert_eq!(report.pending_tool_calls.len(), 1);
            assert_eq!(report.pending_tool_calls[0].name, "confirm");
        });
    }
}
