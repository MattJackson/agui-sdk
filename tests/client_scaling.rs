//! Peer-controlled sizes must not make client-side bookkeeping quadratic:
//! resume validation over many interrupts, answering many pending tool
//! calls, and listing the messages a run added. Each test compares the time
//! for n and 4n items (the pattern of `hostile_peers.rs`): linear code takes
//! about 4x, quadratic code about 16x, and the bound is 8x.
//!
//! Timing tests share a lock so they do not disturb one another.

#![cfg(feature = "client")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_client;

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agui_sdk::client::{AgentClient, RunReport, RunRequest, RunStatus, Thread};
use agui_sdk::diagnostics::NullSink;
use agui_sdk::ids::{MessageId, ToolCallId};
use agui_sdk::interrupt::{Interrupt, ResumeEntry};
use agui_sdk::message::AssistantMessage;
use agui_sdk::pipeline::reduce::PendingToolCall;
use agui_sdk::Message;
use support_client::*;

static SERIAL: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The fastest of three runs of `f` over a fresh `setup()` each time; only
/// `f` is timed.
fn best<S>(setup: impl Fn() -> S, f: impl Fn(S)) -> Duration {
    (0..3)
        .map(|_| {
            let state = setup();
            let started = Instant::now();
            f(state);
            started.elapsed()
        })
        .min()
        .unwrap()
}

fn assert_linear(what: &str, small: Duration, large: Duration) {
    assert!(
        large < small * 8,
        "{what}: 4x the items took {:.1}x the time ({small:?} -> {large:?})",
        large.as_secs_f64() / small.as_secs_f64()
    );
}

fn client() -> AgentClient<MockTransport> {
    AgentClient::with_transport(mock(Vec::new()))
        .diagnostic_sink(NullSink)
        .build()
        .unwrap()
}

#[test]
fn validate_resume_is_linear_in_the_interrupts_and_entries() {
    let _g = lock();
    let time = |n: usize| {
        best(
            || {
                let interrupts: Vec<Interrupt> = (0..n)
                    .map(|k| Interrupt::new(format!("i{k}"), "approval"))
                    .collect();
                let entries: Vec<ResumeEntry> = (0..n)
                    .map(|k| ResumeEntry::cancelled(format!("i{k}")))
                    .collect();
                (
                    client().thread_with_id("t").with_interrupts(interrupts),
                    entries,
                )
            },
            |(thread, entries)| thread.validate_resume(&entries).unwrap(),
        )
    };
    assert_linear("validate_resume", time(3_000), time(12_000));
}

fn pending(n: usize) -> Vec<PendingToolCall> {
    (0..n)
        .map(|k| {
            PendingToolCall::new(
                ToolCallId::from(format!("c{k}")),
                "pick",
                "{}",
                MessageId::from("m1"),
            )
        })
        .collect()
}

fn thread_with_pending(n: usize) -> Thread<MockTransport> {
    let client = client();
    let mut snapshot = client.thread_with_id("t").snapshot();
    snapshot.pending_tool_calls = pending(n);
    Thread::restore(client, snapshot)
}

#[test]
fn answering_pending_calls_is_linear() {
    let _g = lock();
    let time = |n: usize| {
        best(
            || (thread_with_pending(n), pending(n)),
            |(mut thread, calls)| {
                for call in &calls {
                    thread.decline(&call.tool_call_id, "no").unwrap();
                }
                assert!(thread.pending_tool_calls().is_empty());
            },
        )
    };
    assert_linear("decline", time(2_500), time(10_000));
}

#[test]
fn checking_that_every_pending_call_is_answered_is_linear() {
    let _g = lock();
    let time = |n: usize| {
        best(
            || {
                let request = pending(n).iter().fold(RunRequest::new(), |request, call| {
                    request.message(Message::tool(call.tool_call_id.clone(), "done"))
                });
                (thread_with_pending(n), request)
            },
            |(mut thread, request)| {
                let _handle = thread.run(request).unwrap();
            },
        )
    };
    assert_linear("run", time(2_500), time(10_000));
}

#[test]
fn new_messages_is_linear_in_the_history() {
    let _g = lock();
    let report = |n: usize| {
        let mut report = RunReport::new(
            "t".into(),
            "r".into(),
            RunStatus::Finished {
                outcome: None,
                result: None,
            },
        );
        report.messages = (0..n)
            .map(|k| Message::Assistant(AssistantMessage::new(format!("m{k}"))))
            .collect();
        // Every message is new, in reverse order of the history.
        report.new_message_ids = (0..n).rev().map(|k| format!("m{k}").into()).collect();
        report
    };
    let time = |n: usize| {
        best(
            || report(n),
            |report| {
                assert_eq!(report.new_messages().count(), report.messages.len());
            },
        )
    };
    assert_linear("new_messages", time(3_000), time(12_000));
}
