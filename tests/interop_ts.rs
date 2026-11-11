//! Interop with the TypeScript reference (TEST-PLAN §7): the pinned
//! `@ag-ui/client` against this crate's server, and this crate's client
//! against a run produced by the pinned `@ag-ui/encoder`. The Node side lives
//! in `interop/ts/` (excluded from the package).
//!
//! Opt-in: it needs Node and `npm ci` in `interop/ts/`, so it runs only with
//! `AGUI_INTEROP=1` (CI's `ts-interop` job) and is a printed skip otherwise.
//! With the variable set, a missing Node or `node_modules` is a failure.
//!
//! ```text
//! (cd interop/ts && npm ci)
//! AGUI_INTEROP=1 cargo test --features client,axum --test interop_ts
//! ```

#![cfg(all(feature = "client", feature = "axum"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_producer;

use std::future::IntoFuture;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use agui_sdk::client::{AgentClient, RunRequest};
use agui_sdk::server::http::ServeConfig;
use agui_sdk::server::{Agent, BoxError, RunContext, RunOutcome, Runner};
use agui_sdk::Message;
use serde_json::{json, Value};

fn harness() -> Option<PathBuf> {
    if std::env::var("AGUI_INTEROP").as_deref() != Ok("1") {
        eprintln!("SKIPPED: TS interop is opt-in: set AGUI_INTEROP=1 after `npm ci` in interop/ts");
        return None;
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("interop/ts");
    assert!(
        dir.join("node_modules/@ag-ui/client").is_dir(),
        "AGUI_INTEROP=1 but {} has no node_modules: run `npm ci` there",
        dir.display()
    );
    Some(dir)
}

/// A run touching the common event families: state snapshot and delta,
/// text, a backend tool call with its result, a frontend call left pending.
struct Talker;

impl Agent for Talker {
    type State = Value;

    async fn run(&self, mut ctx: RunContext<Value>) -> Result<RunOutcome, BoxError> {
        *ctx.state_mut() = json!({"count": 1});
        ctx.publish_state().await?;
        let step = ctx.emitter().start_step("answer").await?;
        ctx.emitter().say("Hello from Rust").await?;
        let call = ctx.emitter().start_tool_call("lookup", None).await?;
        call.push_args("{\"q\":\"rust\"}").await?;
        let id = call.end().await?;
        ctx.emitter().tool_result(id, "42").await?;
        step.finish().await?;
        *ctx.state_mut() = json!({"count": 2});
        ctx.publish_state().await?;
        // The application's tool, declared by the TS client: left pending.
        ctx.emitter()
            .start_tool_call("confirm", None)
            .await?
            .end()
            .await?;
        Ok(RunOutcome::success())
    }
}

#[test]
fn the_ts_client_accepts_the_rust_server_stream() {
    let Some(dir) = harness() else { return };
    let runtime = support_producer::runtime();
    let mut config = ServeConfig::default();
    // The TS HttpAgent asks for SSE; protobuf stays offered as configured.
    config.protobuf = cfg!(feature = "proto");
    let addr = runtime.block_on(support_producer::serve(Runner::new(Talker), config));
    let output = Command::new("node")
        .arg("consume.mjs")
        .arg(format!("http://{addr}/agent"))
        .current_dir(&dir)
        .output()
        .expect("run node (is Node installed?)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("{stdout}{stderr}");
    assert!(
        output.status.success(),
        "the TS client refused the stream:\n{stderr}"
    );
}

#[test]
fn the_rust_client_accepts_a_ts_sse_stream() {
    rust_client_against_ts(&["--sse"]);
}

/// The client offers protobuf first (feature `proto`, on in the test build),
/// so the TS encoder answers with its protobuf framing. Fails unless the TS
/// encoder actually served protobuf (`--require-proto`).
#[test]
fn the_rust_client_accepts_a_ts_negotiated_stream() {
    rust_client_against_ts(&["--require-proto"]);
}

fn rust_client_against_ts(args: &[&str]) {
    let Some(dir) = harness() else { return };
    let mut child = Command::new("node")
        .arg("produce.mjs")
        .args(args)
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("run node (is Node installed?)");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port = line
        .trim()
        .strip_prefix("LISTENING ")
        .unwrap_or_else(|| panic!("unexpected first line from produce.mjs: {line:?}"));

    let client = AgentClient::http(format!("http://127.0.0.1:{port}/agent"))
        .build()
        .unwrap();
    let request = RunRequest::new()
        .thread_id("t-interop")
        .run_id("r-interop")
        .message(Message::user("hello from Rust"));
    let report = support_producer::runtime().block_on(client.run(request).into_future());
    let status = child.wait().unwrap();

    let report = report.unwrap_or_else(|e| panic!("the Rust client refused the TS stream: {e:?}"));
    assert!(
        status.success(),
        "produce.mjs reported a problem (see its stderr)"
    );
    assert!(report.status.is_success(), "{:?}", report.status);
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    assert_eq!(report.state, json!({"count": 2}));
    let texts: Vec<_> = report.messages.iter().filter_map(Message::text).collect();
    assert!(texts.contains(&"Hello from TypeScript"), "{texts:?}");
    assert!(texts.contains(&"42"), "the tool result: {texts:?}");
    assert_eq!(report.pending_tool_calls.len(), 0);
}
