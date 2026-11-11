//! Helpers for the producer-side rule tests: event builders, a
//! verifier driver that reports the first refusal, a closure agent, and a
//! local axum server plus a raw HTTP/1.1 client over a real TCP socket.

#![allow(dead_code)]

use std::future::Future;
use std::marker::PhantomData;

use agui_sdk::event::Event;
use agui_sdk::pipeline::verify::{ProducerRule, ProducerVerifier};
use agui_sdk::server::{Agent, BoxError, RunContext, RunOutcome};
use serde_json::Value;

pub const THREAD: &str = "t-prod";

/// An event from its wire JSON (the derived, unchecked decoding).
pub fn ev(json: Value) -> Event {
    match serde_json::from_value(json.clone()) {
        Ok(event) => event,
        Err(e) => panic!("not an event: {e}: {json}"),
    }
}

/// `RUN_STARTED` for `run` on the test thread, declaring 1.0.
pub fn started(run: &str) -> Value {
    serde_json::json!({"type": "RUN_STARTED", "threadId": THREAD, "runId": run, "protocolVersion": "1.0"})
}

/// `RUN_FINISHED` for `run` on the test thread.
pub fn finished(run: &str) -> Value {
    serde_json::json!({"type": "RUN_FINISHED", "threadId": THREAD, "runId": run})
}

/// Feeds `events` to a fresh stream-local verifier; `Ok` when all pass, else
/// the index and rule of the first refusal.
pub fn verify(events: &[Value]) -> Result<(), (usize, ProducerRule, String)> {
    verify_with(ProducerVerifier::new(), events)
}

/// As [`verify`], with a given verifier.
pub fn verify_with(
    mut verifier: ProducerVerifier,
    events: &[Value],
) -> Result<(), (usize, ProducerRule, String)> {
    for (i, json) in events.iter().enumerate() {
        if let Err(v) = verifier.check(&ev(json.clone())) {
            return Err((i, v.rule, v.message));
        }
    }
    Ok(())
}

/// Asserts the stream passes.
#[track_caller]
pub fn accepts(events: &[Value]) {
    if let Err((i, rule, message)) = verify(events) {
        panic!("event {i} refused as {rule:?}: {message}");
    }
}

/// Asserts exactly event `at` is refused with `rule`, that the refusal left
/// the verifier unchanged (`next`, if given, is then accepted), and returns
/// the diagnostic.
#[track_caller]
pub fn refuses(events: &[Value], at: usize, rule: ProducerRule) -> String {
    refuses_then(events, at, rule, None)
}

/// As [`refuses`], then checks `next` is accepted after the refusal.
#[track_caller]
pub fn refuses_then(
    events: &[Value],
    at: usize,
    rule: ProducerRule,
    next: Option<Value>,
) -> String {
    let mut verifier = ProducerVerifier::new();
    for (i, json) in events.iter().enumerate() {
        let result = verifier.check(&ev(json.clone()));
        if i < at {
            if let Err(v) = result {
                panic!("event {i} refused early as {:?}: {}", v.rule, v.message);
            }
            continue;
        }
        let Err(v) = result else {
            panic!("event {i} accepted; expected refusal {rule:?}");
        };
        assert_eq!(v.rule, rule, "event {i}: {}", v.message);
        assert!(!v.message.is_empty());
        if let Some(next) = next {
            if let Err(after) = verifier.check(&ev(next)) {
                panic!(
                    "refusal changed the state: {:?}: {}",
                    after.rule, after.message
                );
            }
        }
        return v.message;
    }
    panic!("stream shorter than {at}");
}

/// An agent made from a closure.
pub struct FnAgent<F, Fut>(F, PhantomData<fn() -> Fut>);

/// Wraps a closure `|ctx| async move { … }` as an [`Agent`].
pub fn agent<F, Fut>(f: F) -> FnAgent<F, Fut>
where
    F: Fn(RunContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RunOutcome, BoxError>> + Send + 'static,
{
    FnAgent(f, PhantomData)
}

impl<F, Fut> Agent for FnAgent<F, Fut>
where
    F: Fn(RunContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RunOutcome, BoxError>> + Send + 'static,
{
    type State = Value;

    fn run(&self, ctx: RunContext) -> impl Future<Output = Result<RunOutcome, BoxError>> + Send {
        (self.0)(ctx)
    }
}

/// A current-thread tokio runtime with I/O and timers.
pub fn runtime() -> tokio::runtime::Runtime {
    match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => panic!("runtime: {e}"),
    }
}

#[cfg(feature = "axum")]
/// Serves `runner` at `POST /agent` (and capabilities at `GET
/// /capabilities`) on a fresh local port through the axum glue.
pub async fn serve<A: Agent>(
    runner: agui_sdk::server::Runner<A>,
    config: agui_sdk::server::http::ServeConfig,
) -> std::net::SocketAddr {
    use agui_sdk::server::axum::{respond, AguiRequest, AguiResponse};
    use std::sync::Arc;
    let state = Arc::new((runner, config));
    let app = axum::Router::new()
        .route(
            "/agent",
            axum::routing::post({
                let state = Arc::clone(&state);
                move |req: AguiRequest| {
                    let state = Arc::clone(&state);
                    async move { respond(&state.0, &state.1, req) }
                }
            }),
        )
        .route(
            "/capabilities",
            axum::routing::get({
                let state = Arc::clone(&state);
                move || {
                    let state = Arc::clone(&state);
                    async move { AguiResponse::capabilities(&state.1) }
                }
            }),
        );
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => panic!("bind: {e}"),
    };
    let addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => panic!("addr: {e}"),
    };
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// A raw HTTP/1.1 response.
#[derive(Debug)]
pub struct RawResponse {
    pub status: u16,
    /// Header lines, names lowercased.
    pub headers: Vec<(String, String)>,
    /// The de-chunked body.
    pub body: Vec<u8>,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Sends one request over a fresh TCP connection (`Connection: close`) and
/// reads the whole response, undoing chunked transfer coding.
pub async fn raw_request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> RawResponse {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = match tokio::net::TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => panic!("connect: {e}"),
    };
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    request.extend_from_slice(body);
    if let Err(e) = stream.write_all(&request).await {
        panic!("write: {e}");
    }
    let mut raw = Vec::new();
    if let Err(e) = stream.read_to_end(&mut raw).await {
        panic!("read: {e}");
    }
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> RawResponse {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header end"));
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line"));
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    if chunked {
        body = dechunk(&body);
    }
    RawResponse {
        status,
        headers,
        body,
    }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") {
        let size_text = String::from_utf8_lossy(&data[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("0").trim(), 16)
            .unwrap_or_else(|_| panic!("bad chunk size {size_text:?}"));
        data = &data[line_end + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
    out
}

/// The JSON of each `data:` line of an SSE body, asserting the framing:
/// LF only, every event a single `data:` line followed by a blank line,
/// comments allowed.
pub fn sse_events(body: &[u8]) -> Vec<Value> {
    assert!(!body.contains(&b'\r'), "SSE body contains CR");
    let text = std::str::from_utf8(body).unwrap_or_else(|e| panic!("utf-8: {e}"));
    let mut events = Vec::new();
    for block in text.split("\n\n").filter(|b| !b.is_empty()) {
        let mut data = Vec::new();
        for line in block.split('\n') {
            if let Some(payload) = line.strip_prefix("data: ") {
                data.push(payload);
            } else {
                assert!(line.starts_with(':'), "unexpected SSE line {line:?}");
            }
        }
        if data.is_empty() {
            continue;
        }
        assert_eq!(data.len(), 1, "one data line per event");
        events.push(serde_json::from_str(data[0]).unwrap_or_else(|e| panic!("{e}: {}", data[0])));
    }
    events
}

/// The event types of a list of wire events.
pub fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A minimal valid request body.
pub fn input_json(thread: &str, run: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "threadId": thread, "runId": run, "protocolVersion": "1.0",
        "messages": [{"id": "u1", "role": "user", "content": "hi"}],
        "state": {}, "tools": [], "context": [], "forwardedProps": {}
    }))
    .unwrap_or_default()
}
