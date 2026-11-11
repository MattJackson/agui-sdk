//! The HTTP+SSE binding, client side (`transports/http-sse.mdx`): what the
//! HTTP transport sends, how it reads `text/event-stream`, and how the end
//! of a body is judged. Driven against a local HTTP/1.1 mock server.

mod transport_support;

use agui_sdk::client::transport::sse::{SseDecoder, DEFAULT_MAX_EVENT_BYTES};
use agui_sdk::client::transport::{HttpTransport, Transport};
use agui_sdk::pipeline::Termination;
use agui_sdk::wire::Chunk;
use agui_sdk::ErrorCode;
use serde_json::Value;
use std::time::Duration;
use transport_support::*;

fn transport(server: &MockServer) -> HttpTransport {
    HttpTransport::builder(server.url("/agent"))
        .build()
        .unwrap_or_else(|e| panic!("{e}"))
}

/// Serves `body` as SSE (in `piece`-byte pieces) and returns the frames.
fn serve_sse(body: Vec<u8>, piece: usize) -> (Vec<Value>, Option<agui_sdk::Error>, Captured) {
    block(async move {
        let server = MockServer::always(Reply::sse(body).pieces(piece, Duration::ZERO))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let stream = transport(&server)
            .open(request())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let (frames, error) = drain(stream).await;
        let captured = server.last_request().unwrap_or_default();
        (frames, error, captured)
    })
}

/// What the application is told: an `AgentClient` over the HTTP transport,
/// run against a server that always sends `reply`.
fn client_outcome(reply: Reply) -> Result<agui_sdk::client::RunReport, agui_sdk::Error> {
    use std::future::IntoFuture;
    block(async move {
        let server = MockServer::always(reply)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let client = agui_sdk::client::AgentClient::with_transport(transport(&server))
            .diagnostic_sink(agui_sdk::diagnostics::NullSink)
            .build()
            .unwrap_or_else(|e| panic!("{e}"));
        let request = agui_sdk::client::RunRequest::new()
            .thread_id("t1")
            .run_id("r1")
            .user("hi");
        client.run(request).into_future().await
    })
}

/// covers: SSE-01
#[test]
fn posts_the_input_as_one_json_object() {
    let (_, error, req) = serve_sse(sse(&events()), 0);
    assert!(error.is_none());
    assert_eq!(req.method, "POST");
    assert_eq!(req.target, "/agent");
    assert_eq!(req.header("content-type"), Some("application/json"));
    assert_eq!(req.json(), Some(input()));
    let length: usize = req
        .header("content-length")
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    assert_eq!(
        length,
        req.body.len(),
        "the body is sent whole, with its length"
    );
}

/// covers: SSE-02
#[test]
fn accepts_the_event_stream_explicitly() {
    let (_, _, req) = serve_sse(sse(&events()), 0);
    let accept = req.header("accept").unwrap_or_default().to_owned();
    assert!(!accept.contains('*'), "never a wildcard: {accept}");
    if cfg!(feature = "proto") {
        assert_eq!(
            accept,
            "text/event-stream, application/vnd.ag-ui.event+proto"
        );
    } else {
        assert_eq!(accept, "text/event-stream");
    }
    assert_eq!(req.header_all("accept").len(), 1);
}

/// covers: SSE-04
#[test]
fn each_event_is_one_json_object_and_multi_line_data_is_joined() {
    // The second event's JSON is spread over three `data:` lines.
    let mut body = sse(&events()[..1]);
    body.extend_from_slice(
        b"data: {\"type\":\"TEXT_MESSAGE_START\",\ndata: \"messageId\":\"m-r1\",\ndata: \"role\":\"assistant\"}\n\n",
    );
    body.extend_from_slice(&sse(&events()[2..]));
    for piece in [0, 1, 3, 7] {
        let (frames, error, _) = serve_sse(body.clone(), piece);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(frames, events(), "piece {piece}");
    }
    // The lines are joined with LF, exactly, into one event's data.
    let mut decoder = SseDecoder::new(DEFAULT_MAX_EVENT_BYTES);
    decoder.push(Chunk::from_static(
        b"data: {\"a\":\ndata: 1}\n\ndata: [\n\n",
    ));
    let joined = decoder.next_event().ok().flatten().unwrap_or_default();
    assert_eq!(&*joined, b"{\"a\":\n1}");
    let lone = decoder.next_event().ok().flatten().unwrap_or_default();
    assert_eq!(
        &*lone, b"[",
        "a fragment is passed on as is, for the pipeline to refuse"
    );
}

/// covers: SSE-04
#[test]
fn an_event_with_empty_data_is_a_frame_that_maps_to_no_event() {
    // WHATWG dispatches `data:` as an event with data `""`; it carries no
    // protocol event, so the run fails exactly as for two empty lines.
    let told = |empty: &[u8]| {
        let mut body = sse(&events()[..1]);
        body.extend_from_slice(empty);
        body.extend_from_slice(&sse(&events()[1..]));
        match client_outcome(Reply::sse(body)) {
            Ok(_) => panic!("{empty:?}: the run succeeded"),
            Err(err) => err.code(),
        }
    };
    let joined = told(b"data:\ndata:\n\n");
    for empty in [&b"data:\n\n"[..], b"data\n\n", b"data: \r\n\r\n"] {
        assert_eq!(told(empty), joined, "{empty:?}");
    }
}

/// covers: SSE-06
#[test]
fn reads_cr_and_crlf_line_endings() {
    let lf = String::from_utf8(sse(&events())).unwrap_or_default();
    for ending in ["\r", "\r\n"] {
        let body = lf.replace('\n', ending).into_bytes();
        for piece in [0, 1, 5] {
            let (frames, error, _) = serve_sse(body.clone(), piece);
            assert!(error.is_none());
            assert_eq!(frames, events(), "{ending:?} piece {piece}");
        }
    }
}

/// covers: SSE-07
#[test]
fn ignores_fields_other_than_data() {
    let mut body = Vec::new();
    for (i, e) in events().iter().enumerate() {
        body.extend_from_slice(
            format!("event: agui\nid: {i}\nretry: 10\nfoo: bar\ndata: {e}\n\n").as_bytes(),
        );
    }
    body.extend_from_slice(b"event: only-a-name\nid: 9\n\n");
    let (frames, error, _) = serve_sse(body, 4);
    assert!(error.is_none());
    assert_eq!(frames, events());
}

/// covers: SSE-08
#[test]
fn tolerates_comment_lines() {
    let mut body = b": keep-alive\n\n".to_vec();
    for e in events() {
        body.extend_from_slice(format!(":ping\ndata: {e}\n: between\n\n").as_bytes());
        body.extend_from_slice(b": keep-alive\n\n");
    }
    let (frames, error, _) = serve_sse(body, 2);
    assert!(error.is_none());
    assert_eq!(frames, events());
    assert_eq!(judge(&frames).ok(), Some(Termination::Complete));
}

/// covers: SSE-11
#[test]
fn reads_several_runs_in_one_response() {
    let mut script = run("t1", "r0", "replayed");
    script.extend(run("t1", "r1", "requested"));
    let (frames, error, _) = serve_sse(sse(&script), 9);
    assert!(error.is_none());
    assert_eq!(frames, script);
    assert_eq!(judge(&frames).ok(), Some(Termination::Complete));
}

/// covers: SSE-14
#[test]
fn a_200_alone_is_not_success() {
    // Status 200 and the right media type, but no events at all, or events
    // without a terminal one: the body ends cleanly for the transport and
    // the pipeline calls the run truncated, never complete.
    for script in [Vec::new(), events()[..4].to_vec()] {
        let (frames, error, _) = serve_sse(sse(&script), 0);
        assert!(error.is_none());
        assert_eq!(frames, script);
        assert_eq!(judge(&frames).ok(), Some(Termination::Truncated));
        // What the application is told: not success, but a truncated run.
        let told = client_outcome(Reply::sse(sse(&script)));
        assert!(
            matches!(&told, Err(e) if e.code() == ErrorCode::Truncated),
            "{script:?}: {told:?}"
        );
    }
    // The same server with the terminal event present is a success.
    let told = client_outcome(Reply::sse(sse(&events())));
    assert!(
        told.as_ref().is_ok_and(|r| r.status.is_success()),
        "{told:?}"
    );
}

/// covers: SSE-15
#[test]
fn a_dropped_connection_is_a_truncated_run() {
    let body = sse(&events());
    let at_boundary = sse(&events()[..3]).len();
    // Cut between events: the delivered events stand, the run is truncated.
    let (frames, error) = block(async {
        let server = MockServer::always(
            Reply::sse(body.clone())
                .framing(Framing::Length)
                .cut_after(at_boundary),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let stream = transport(&server)
            .open(request())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        drain(stream).await
    });
    assert!(error.is_none(), "{error:?}");
    assert_eq!(frames, events()[..3].to_vec());
    assert_eq!(judge(&frames).ok(), Some(Termination::Truncated));
    // The application is told the run was truncated, and keeps what arrived.
    let told = client_outcome(
        Reply::sse(body.clone())
            .framing(Framing::Length)
            .cut_after(at_boundary),
    );
    let err = match told {
        Ok(report) => panic!("a dropped connection read as completed: {report:?}"),
        Err(err) => err,
    };
    assert_eq!(err.code(), ErrorCode::Truncated, "{err}");
    let partial = err
        .partial()
        .unwrap_or_else(|| panic!("a truncated run has a partial report"));
    assert_eq!(
        partial.messages.last().and_then(|m| m.text()),
        Some("Hello")
    );

    // Cut inside an event, under each framing: a truncation error.
    for framing in [Framing::Chunked, Framing::Length, Framing::Close] {
        let (frames, error) = block(async {
            let server = MockServer::always(
                Reply::sse(body.clone())
                    .framing(framing)
                    .cut_after(at_boundary + 10),
            )
            .await
            .unwrap_or_else(|e| panic!("{e}"));
            let stream = transport(&server)
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            drain(stream).await
        });
        assert_eq!(frames, events()[..3].to_vec(), "{framing:?}");
        let error = error.unwrap_or_else(|| panic!("{framing:?}: no error"));
        assert_eq!(error.code(), ErrorCode::Truncated, "{framing:?}: {error}");
        let told = client_outcome(
            Reply::sse(body.clone())
                .framing(framing)
                .cut_after(at_boundary + 10),
        );
        assert!(
            matches!(&told, Err(e) if e.code() == ErrorCode::Truncated),
            "{framing:?}: {told:?}"
        );
    }
}

/// covers: SSE-16
#[test]
fn never_resumes_and_sends_no_last_event_id() {
    let mut body = Vec::new();
    for (i, e) in events()[..3].iter().enumerate() {
        body.extend_from_slice(format!("id: {i}\nretry: 1\ndata: {e}\n\n").as_bytes());
    }
    block(async move {
        let server = MockServer::always(Reply::sse(body).framing(Framing::Length))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = transport(&server);
        let stream = transport
            .open(request())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let (frames, error) = drain(stream).await;
        assert!(error.is_none());
        assert_eq!(frames.len(), 3);
        // The truncated run is not reconnected: one request, and a re-run is
        // a fresh request with no resumption header.
        sleep(50).await;
        assert_eq!(server.requests().len(), 1);
        let stream = transport
            .open(request())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let _ = drain(stream).await;
        for req in server.requests() {
            assert_eq!(req.header("last-event-id"), None);
        }
        assert_eq!(server.requests().len(), 2);
    });
}

/// The decoder alone: every split of a stream into chunks yields the same
/// events, as zero-copy slices where an event's line lies in one chunk.
#[test]
fn decoder_is_chunking_invariant() {
    let body =
        b"\xEF\xBB\xBF: c\r\ndata: {\"a\":1}\r\n\r\ndata: {\"b\":\ndata: 2}\rid: x\r\rdata:{}\n\n";
    let expected: Vec<&[u8]> = vec![b"{\"a\":1}", b"{\"b\":\n2}", b"{}"];
    for size in 1..body.len() {
        let mut decoder = SseDecoder::new(DEFAULT_MAX_EVENT_BYTES);
        let mut got = Vec::new();
        for piece in body.chunks(size) {
            decoder.push(Chunk::copy_from_slice(piece));
            while let Some(event) = decoder.next_event().unwrap_or_else(|e| panic!("{e}")) {
                got.push(event.to_vec());
            }
        }
        assert!(!decoder.has_partial());
        let got: Vec<&[u8]> = got.iter().map(Vec::as_slice).collect();
        assert_eq!(got, expected, "chunk size {size}");
    }
    let whole = Chunk::from_static(b"data: {\"x\":true}\n\n");
    let mut decoder = SseDecoder::new(DEFAULT_MAX_EVENT_BYTES);
    decoder.push(whole.clone());
    let event = decoder.next_event().ok().flatten().unwrap_or_default();
    assert_eq!(event.as_ptr(), whole[6..].as_ptr(), "a slice, not a copy");
}
