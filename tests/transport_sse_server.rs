//! The SSE binding, server side (`transports/http-sse.mdx`): status and
//! media type, framing, keep-alives, closing, several runs, and the two error
//! paths. Exercised over a real local TCP socket through the axum glue, and
//! on the encoded bytes directly.

#![cfg(all(feature = "server", feature = "axum"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_producer;

use std::time::Duration;

use agui_sdk::ids::RunId;
use agui_sdk::server::encode::Encoder;
use agui_sdk::server::http::ServeConfig;
use agui_sdk::server::input::{admit, InputPolicy};
use agui_sdk::server::{CancelToken, Extensions, ReplayedRun, RunOutcome, Runner};
use agui_sdk::wire::WireFormat;
use serde_json::json;
use support_producer::{agent, input_json, raw_request, runtime, serve, sse_events, types};

fn sse_only() -> ServeConfig {
    let mut config = ServeConfig::default();
    config.protobuf = false;
    config
}

const POST_HEADERS: [(&str, &str); 2] = [
    ("Content-Type", "application/json"),
    ("Accept", "text/event-stream"),
];

fn talker() -> Runner<impl agui_sdk::server::Agent<State = serde_json::Value>> {
    Runner::new(agent(|ctx| async move {
        ctx.emitter()
            .say("line one\nline two\r\nline three")
            .await?;
        Ok(RunOutcome::success())
    }))
}

/// covers: SSE-03, SSE-10
#[test]
fn a_started_run_answers_200_event_stream_and_closes_after_the_terminal() {
    runtime().block_on(async {
        let addr = serve(talker(), sse_only()).await;
        let response = raw_request(
            addr,
            "POST",
            "/agent",
            &POST_HEADERS,
            &input_json("t1", "r1"),
        )
        .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.header("content-type"), Some("text/event-stream"));
        assert_eq!(response.header("vary"), Some("Accept"));
        // The connection closed by itself, right after the terminal event.
        let events = sse_events(&response.body);
        assert_eq!(
            types(&events).last().map(String::as_str),
            Some("RUN_FINISHED")
        );
        let text = String::from_utf8(response.body).unwrap();
        assert!(text.ends_with("\n\n"), "{text:?}");
    });
}

/// An `Accept` admitting neither binding is still answered, in SSE: the
/// protobuf binding says a producer MUST answer SSE whenever it does not
/// answer protobuf, so there is no 406.
///
/// covers: PB-05
#[test]
fn an_accept_admitting_neither_binding_is_answered_with_sse() {
    runtime().block_on(async {
        let addr = serve(talker(), ServeConfig::default()).await;
        for accept in ["application/json", "*/*;q=0"] {
            let headers = [("Content-Type", "application/json"), ("Accept", accept)];
            let response =
                raw_request(addr, "POST", "/agent", &headers, &input_json("t1", "r1")).await;
            assert_eq!(response.status, 200, "{accept}");
            assert_eq!(
                response.header("content-type"),
                Some("text/event-stream"),
                "{accept}"
            );
            assert_eq!(
                types(&sse_events(&response.body))
                    .last()
                    .map(String::as_str),
                Some("RUN_FINISHED")
            );
        }
    });
}

/// covers: SSE-04, SSE-05
#[test]
fn each_event_is_one_data_line_of_one_json_object_framed_with_lf() {
    runtime().block_on(async {
        let addr = serve(talker(), sse_only()).await;
        let response = raw_request(
            addr,
            "POST",
            "/agent",
            &POST_HEADERS,
            &input_json("t1", "r1"),
        )
        .await;
        // `sse_events` asserts: no CR anywhere, one `data:` line per event,
        // each a whole JSON object.
        let events = sse_events(&response.body);
        let content = events
            .iter()
            .find(|e| e["type"] == "TEXT_MESSAGE_CONTENT")
            .unwrap();
        assert_eq!(content["delta"], "line one\nline two\r\nline three");
    });
    // The encoder writes exactly `data: <json>\n\n`.
    let mut out = Vec::new();
    Encoder::for_format(WireFormat::Sse)
        .encode(
            &agui_sdk::Event::from(agui_sdk::lifecycle::RunError::new("x")),
            &mut out,
        )
        .unwrap();
    assert_eq!(out, b"data: {\"type\":\"RUN_ERROR\",\"message\":\"x\"}\n\n");
}

/// covers: SSE-09
#[test]
fn keep_alive_comments_fill_quiet_periods() {
    let slow = Runner::new(agent(|ctx| async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        ctx.emitter().say("done").await?;
        Ok(RunOutcome::success())
    }));
    let mut config = sse_only();
    config.keepalive = Some(Duration::from_millis(40));
    runtime().block_on(async {
        let addr = serve(slow, config).await;
        let response = raw_request(
            addr,
            "POST",
            "/agent",
            &POST_HEADERS,
            &input_json("t1", "r1"),
        )
        .await;
        let text = String::from_utf8(response.body.clone()).unwrap();
        assert!(text.contains(": keep-alive\n\n"), "{text}");
        // Comments do not disturb the events.
        assert_eq!(
            types(&sse_events(&response.body))
                .last()
                .map(String::as_str),
            Some("RUN_FINISHED")
        );
    });
}

/// covers: SSE-11
#[test]
fn one_response_may_carry_several_runs() {
    let runner = talker();
    let admitted = admit(&input_json("t1", "r-now"), &InputPolicy::default()).unwrap();
    let replay = vec![ReplayedRun::new(
        RunId::new("r-before"),
        Vec::new(),
        json!({}),
    )];
    let mut stream = runner
        .run_with_replay(replay, admitted, CancelToken::new(), Extensions::new())
        .unwrap();
    let mut encoder = Encoder::for_format(WireFormat::Sse);
    let body = runtime().block_on(async move {
        let mut out = Vec::new();
        while let Some(event) = stream.next().await {
            encoder.encode(&event, &mut out).unwrap();
        }
        out
    });
    let events = sse_events(&body);
    let runs: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "RUN_STARTED")
        .map(|e| e["runId"].as_str().unwrap())
        .collect();
    assert_eq!(runs, ["r-before", "r-now"]);
}

/// covers: SSE-12
#[test]
fn input_rejected_before_start_is_an_http_error_without_a_stream() {
    runtime().block_on(async {
        let addr = serve(talker(), sse_only()).await;
        for (headers, body, status) in [
            (&POST_HEADERS[..], &b"{not json"[..], 400),
            (
                &[
                    ("Content-Type", "text/plain"),
                    ("Accept", "text/event-stream"),
                ][..],
                &input_json("t1", "r1")[..],
                415,
            ),
        ] {
            let response = raw_request(addr, "POST", "/agent", headers, body).await;
            assert_eq!(response.status, status);
            assert_eq!(response.header("content-type"), Some("application/json"));
            let error: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            assert!(
                error["code"].as_str().unwrap().starts_with("server."),
                "{error}"
            );
            assert!(!String::from_utf8_lossy(&response.body).contains("data:"));
        }
    });
}

/// covers: SSE-13
#[test]
fn a_failure_after_the_stream_opens_is_an_in_stream_run_error() {
    let failing = Runner::new(agent(|ctx| async move {
        ctx.emitter().say("partial").await?;
        Err("backend unavailable".into())
    }));
    runtime().block_on(async {
        let addr = serve(failing, sse_only()).await;
        let response = raw_request(
            addr,
            "POST",
            "/agent",
            &POST_HEADERS,
            &input_json("t1", "r1"),
        )
        .await;
        assert_eq!(response.status, 200);
        let events = sse_events(&response.body);
        assert_eq!(types(&events).last().map(String::as_str), Some("RUN_ERROR"));
    });
}

/// covers: SSE-16
#[test]
fn there_is_no_resumption_and_a_rerun_is_a_new_run() {
    runtime().block_on(async {
        let addr = serve(talker(), sse_only()).await;
        let mut headers = POST_HEADERS.to_vec();
        headers.push(("Last-Event-ID", "42"));
        let first = raw_request(addr, "POST", "/agent", &headers, &input_json("t1", "r1")).await;
        let again = raw_request(addr, "POST", "/agent", &headers, &input_json("t1", "r2")).await;
        let text = String::from_utf8(first.body.clone()).unwrap();
        // Events carry no SSE `id:` field to resume from...
        assert!(!text.lines().any(|l| l.starts_with("id:")), "{text}");
        // ...and the header changes nothing: each request is a whole run.
        for (response, run) in [(first, "r1"), (again, "r2")] {
            let events = sse_events(&response.body);
            assert_eq!(events[0]["type"], "RUN_STARTED");
            assert_eq!(events[0]["runId"], run);
        }
    });
}

/// An `AguiResponse` built from `http::handle`'s response (`From`) keeps the
/// keep-alive interval of the `ServeConfig` it was handled with, not the
/// 15 s default.
#[test]
fn a_converted_response_keeps_the_configured_keep_alive() {
    use agui_sdk::server::axum::AguiResponse;
    use agui_sdk::server::http::{handle, HttpRequest};
    use axum::response::IntoResponse;

    let slow = Runner::new(agent(|ctx| async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        ctx.emitter().say("done").await?;
        Ok(RunOutcome::success())
    }));
    let mut config = sse_only();
    config.keepalive = Some(Duration::from_millis(40));
    let body = input_json("t1", "r1");
    let request = HttpRequest::new(Some("application/json"), Some("text/event-stream"), &body);
    let response = handle(&slow, &config, request, CancelToken::new()).unwrap();
    let text = runtime().block_on(async move {
        let response = AguiResponse::from(response).into_response();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    });
    assert!(text.contains(": keep-alive\n\n"), "{text}");
}

/// covers: PB-05
#[cfg(feature = "proto")]
#[test]
fn accept_lines_are_read_as_one_list() {
    const SSE: &str = "text/event-stream";
    const PROTO: &str = "application/vnd.ag-ui.event+proto";
    // Both orders: a first-line-only or a last-line-only reader fails one.
    for (first, second) in [(SSE, PROTO), (PROTO, SSE)] {
        runtime().block_on(async {
            let addr = serve(talker(), ServeConfig::default()).await;
            let response = raw_request(
                addr,
                "POST",
                "/agent",
                &[
                    ("Content-Type", "application/json"),
                    ("Accept", first),
                    ("Accept", second),
                ],
                &input_json("t1", "r1"),
            )
            .await;
            assert_eq!(response.status, 200, "order {first}, {second}");
            assert_eq!(
                response.header("content-type"),
                Some(PROTO),
                "order {first}, {second}"
            );
        });
    }
}

/// Serves `talker()` with axum's own body limit disabled, as the
/// `server::axum` module docs allow.
async fn serve_without_axum_limit() -> std::net::SocketAddr {
    use agui_sdk::server::axum::{respond, AguiRequest};
    use std::sync::Arc;
    let state = Arc::new((talker(), ServeConfig::default()));
    let app = axum::Router::new()
        .route(
            "/agent",
            axum::routing::post(move |req: AguiRequest| {
                let state = Arc::clone(&state);
                async move { respond(&state.0, &state.1, req) }
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::disable());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// Reads until the response's status line is complete, or the connection
/// closes or resets; `None` when no status line arrived.
async fn read_status(stream: &mut tokio::net::TcpStream) -> Option<u16> {
    use tokio::io::AsyncReadExt;
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(2).any(|w| w == b"\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&head)
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
}

/// With axum's limit disabled, a `Content-Length` over the extractor's
/// 16 MiB read cap is answered `413` before any of the body is read
/// (docs/DECISIONS.md D20).
#[test]
fn axum_rejects_an_oversized_content_length_before_reading() {
    use tokio::io::AsyncWriteExt;
    runtime().block_on(async {
        let addr = serve_without_axum_limit().await;
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "POST /agent HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Accept: text/event-stream\r\nContent-Length: 1073741824\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), read_status(&mut stream)).await;
        assert_eq!(status, Ok(Some(413)), "answered without reading the body");
    });
}

/// With axum's limit disabled, a chunked body is read no further than the
/// extractor's 16 MiB cap: the `413` (or a reset) arrives before the 64 MiB
/// the client would send has all gone out, so the body is never buffered
/// whole.
#[test]
fn axum_stops_reading_a_chunked_body_at_the_cap() {
    use tokio::io::AsyncWriteExt;
    const MIB: usize = 1024 * 1024;
    runtime().block_on(async {
        let addr = serve_without_axum_limit().await;
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut read, mut write) = stream.into_split();
        let writer = tokio::spawn(async move {
            let head = format!(
                "POST /agent HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
                 Accept: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
            );
            write.write_all(head.as_bytes()).await?;
            let chunk = vec![b' '; MIB];
            for _ in 0..64 {
                write.write_all(format!("{MIB:x}\r\n").as_bytes()).await?;
                write.write_all(&chunk).await?;
                write.write_all(b"\r\n").await?;
            }
            write.write_all(b"0\r\n\r\n").await?;
            Ok::<_, std::io::Error>(())
        });
        let status = tokio::time::timeout(Duration::from_secs(20), async {
            use tokio::io::AsyncReadExt;
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(2).any(|w| w == b"\r\n") {
                match read.read(&mut buf).await {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            String::from_utf8_lossy(&head)
                .split(' ')
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok())
        })
        .await
        .unwrap();
        // The status is decided before the whole body went out: the writer
        // either failed (the server stopped reading and closed) or is still
        // blocked on a full socket.
        let finished_sending = writer.is_finished() && matches!(writer.await, Ok(Ok(())));
        assert!(
            !finished_sending,
            "the server read all 64 MiB before answering"
        );
        // A `413`, or a reset that discarded it (the server closes with the
        // rest of the body unread).
        assert!(matches!(status, Some(413) | None), "{status:?}");
    });
}

/// axum's own `DefaultBodyLimit` (2 MiB by default) still answers `413`
/// through the extractor's cap.
///
/// The request declares 3 MiB but sends one byte over the limit and then
/// waits: the server has read everything that was sent when it answers, so
/// it closes cleanly instead of resetting the connection with body bytes
/// unread (a reset can discard the `413` before the client reads it).
#[test]
fn axum_default_body_limit_still_answers_413() {
    use tokio::io::AsyncWriteExt;
    const LIMIT: usize = 2 * 1024 * 1024;
    runtime().block_on(async {
        let addr = serve(talker(), sse_only()).await;
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "POST /agent HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Accept: text/event-stream\r\nContent-Length: {}\r\n\r\n",
            3 * 1024 * 1024
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(&vec![b' '; LIMIT + 1]).await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), read_status(&mut stream)).await;
        assert_eq!(status, Ok(Some(413)));
    });
}
