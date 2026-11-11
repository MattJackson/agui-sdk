//! Interrupts and resume, consumer side (`patterns/interrupt-resume.mdx`):
//! `Thread` checks coverage, uniqueness and expiry before anything is sent.

#![cfg(feature = "client")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_client;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agui_sdk::client::{AgentClient, RunRequest, RunStatus, Thread};
use agui_sdk::diagnostics::{CollectingSink, DiagnosticCode, NullSink};
use agui_sdk::interrupt::{Interrupt, ResumeEntry};
use agui_sdk::{ErrorCode, ErrorKind};
use serde_json::{json, Value};
use support_client::*;

/// 2026-01-01T00:00:00Z.
const JAN_1: u64 = 1_767_225_600;

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn interrupted(run: &str, interrupts: Value) -> Vec<Value> {
    vec![
        run_started("t1", run),
        json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": run,
               "outcome": {"type": "interrupt", "interrupts": interrupts}}),
    ]
}

/// A thread whose next run is served `frames`, already holding `open`.
fn thread_with(frames: Vec<Value>, open: Vec<Interrupt>) -> (Thread<MockTransport>, MockTransport) {
    let transport = mock(frames);
    let client = AgentClient::with_transport(transport.clone())
        .diagnostic_sink(NullSink)
        .build()
        .unwrap();
    let thread = client
        .thread_with_id("t1")
        .with_interrupts(open)
        .with_clock(Arc::new(|| at(JAN_1)));
    (thread, transport)
}

fn two_open() -> Vec<Interrupt> {
    vec![
        Interrupt::new("i1", "approval"),
        Interrupt::new("i2", "credential"),
    ]
}

/// covers: INTR-05, INTR-06
#[test]
fn interrupts_are_carried_as_sent_into_the_report_and_the_thread() {
    let frames = interrupted(
        "r1",
        json!([{"id": "i1", "reason": "x-vendor-reason", "message": "Approve?",
                "toolCallId": "tc1", "responseSchema": {"type": "boolean"},
                "subagentRunId": "sub-1", "expiresAt": "2026-01-01T00:10:00Z"}]),
    );
    let (mut thread, _) = thread_with(frames, vec![]);
    let report =
        block_on(thread.run_and_absorb(RunRequest::new().run_id("r1").user("go"))).unwrap();
    assert!(report.status.is_interrupted());
    let open = &thread.open_interrupts()[0];
    assert_eq!(open.reason, "x-vendor-reason");
    assert_eq!(open.message.as_deref(), Some("Approve?"));
    assert_eq!(open.tool_call_id.as_ref().unwrap().as_str(), "tc1");
    assert_eq!(
        open.response_schema.as_ref().unwrap().get("type"),
        Some(&json!("boolean"))
    );
    assert_eq!(open.subagent_run_id.as_ref().unwrap().as_str(), "sub-1");
    assert_eq!(report.interrupts, thread.open_interrupts());
}

/// covers: INTR-07
#[test]
fn an_interrupted_run_is_closed_and_continuing_is_a_new_run() {
    let mut frames = interrupted("r1", json!([{"id": "i1", "reason": "approval"}]));
    // The one late arrival a closed run admits.
    frames.push(json!({"type": "RUN_ERROR", "message": "late failure"}));
    let (mut thread, _) = thread_with(frames, vec![]);
    let report = block_on(thread.run_and_absorb(RunRequest::new().run_id("r1"))).unwrap();
    assert!(matches!(report.status, RunStatus::Errored(_)));
    // The failed run does not end awaiting answers: its interrupts are gone,
    // and the thread keeps what it held before (nothing).
    assert!(report.interrupts.is_empty(), "{:?}", report.interrupts);
    assert!(thread.open_interrupts().is_empty());
    // A second run on the same id is a different run in the API: the thread
    // refuses to start one while the first is unabsorbed, and each run is
    // its own handle with its own id.
    let (mut thread, _) = thread_with(
        interrupted("r1", json!([{"id": "i1", "reason": "a"}])),
        vec![],
    );
    let _handle = thread.run(RunRequest::new().run_id("r1")).unwrap();
    let err = thread.run(RunRequest::new().run_id("r2")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidRequest);
}

/// covers: INTR-08
#[test]
fn an_entry_naming_no_open_interrupt_is_refused_before_sending() {
    let (mut thread, transport) = thread_with(vec![], two_open());
    let err = thread
        .resume(vec![
            ResumeEntry::resolved("i1", json!(true)),
            ResumeEntry::cancelled("i2"),
            ResumeEntry::cancelled("i-unknown"),
        ])
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::ResumeCoverage);
    assert_eq!(err.kind(), ErrorKind::Conversation);
    assert_eq!(err.pointer(), Some("/resume/2/interruptId"));
    assert!(transport.requests().is_empty());
}

/// covers: INTR-08, INTR-10
#[test]
fn an_interrupt_answered_twice_is_refused() {
    let (thread, _) = thread_with(vec![], two_open());
    let err = thread
        .validate_resume(&[
            ResumeEntry::resolved("i1", json!(1)),
            ResumeEntry::cancelled("i1"),
            ResumeEntry::cancelled("i2"),
        ])
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::ResumeCoverage);
    assert!(
        err.message().contains("more than once"),
        "{}",
        err.message()
    );
}

/// covers: INTR-09
#[test]
fn entries_are_sent_with_their_status_payload_and_metadata() {
    let (mut thread, transport) = thread_with(run_of("t1", "r2", vec![]), two_open());
    let request = RunRequest::new()
        .run_id("r2")
        .resume(ResumeEntry::resolved(
            "i1",
            json!({"approved": true, "note": null}),
        ))
        .resume(ResumeEntry::cancelled("i2").with_metadata(
            serde_json::from_value(json!({"signature": "sig-1", "routing": null})).unwrap(),
        ));
    block_on(thread.run_and_absorb(request)).unwrap();
    let sent = transport.last_request().unwrap();
    assert_eq!(
        sent["resume"],
        json!([
            {"interruptId": "i1", "status": "resolved", "payload": {"approved": true, "note": null}},
            {"interruptId": "i2", "status": "cancelled",
             "metadata": {"signature": "sig-1", "routing": null}}
        ])
    );
}

/// covers: INTR-10, INTR-11, INTR-12
#[test]
fn omission_is_not_abandonment_and_nothing_is_sent() {
    let (mut thread, transport) = thread_with(vec![], two_open());
    let err = thread
        .resume(vec![ResumeEntry::resolved("i1", json!(true))])
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::ResumeCoverage);
    assert!(err.message().contains("i2"), "{}", err.message());
    // A plain send past open interrupts is the same omission.
    let err = thread
        .send(agui_sdk::Message::user("ignore that"))
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::ResumeCoverage);
    assert!(transport.requests().is_empty());
    assert!(thread.validate_resume(&[]).is_err());
}

/// covers: INTR-14
#[test]
fn a_resolved_entry_for_an_expired_interrupt_is_refused() {
    let open = vec![
        Interrupt::new("past", "a").with_expires_at("2025-12-31T23:59:59Z"),
        Interrupt::new("now", "b").with_expires_at("2026-01-01T00:00:00Z"),
        Interrupt::new("future", "c").with_expires_at("2026-01-01T00:00:01Z"),
    ];
    let (thread, _) = thread_with(vec![], open);
    let all = |past: ResumeEntry, now: ResumeEntry| {
        vec![past, now, ResumeEntry::resolved("future", json!(1))]
    };
    let err = thread
        .validate_resume(&all(
            ResumeEntry::resolved("past", json!(1)),
            ResumeEntry::cancelled("now"),
        ))
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InterruptExpired);
    assert_eq!(err.pointer(), Some("/resume/0/status"));
    // Now-or-earlier is expired (the reference reading).
    let err = thread
        .validate_resume(&all(
            ResumeEntry::cancelled("past"),
            ResumeEntry::resolved("now", json!(1)),
        ))
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InterruptExpired);
    thread
        .validate_resume(&all(
            ResumeEntry::cancelled("past"),
            ResumeEntry::cancelled("now"),
        ))
        .unwrap();
}

/// covers: INTR-14
#[test]
fn expiry_is_judged_with_the_threads_clock() {
    let open = vec![Interrupt::new("i1", "a").with_expires_at("2026-01-01T00:00:00Z")];
    let (thread, _) = thread_with(vec![], open);
    let entry = [ResumeEntry::resolved("i1", json!(1))];
    assert!(thread.validate_resume(&entry).is_err());
    let thread = thread.with_clock(Arc::new(|| at(JAN_1 - 1)));
    thread.validate_resume(&entry).unwrap();
}

/// covers: INTR-14
#[test]
fn an_unparseable_expiry_never_expires_and_warns() {
    let sink = Arc::new(CollectingSink::new());
    let client = AgentClient::with_transport(mock(vec![]))
        .diagnostic_sink(Arc::clone(&sink))
        .build()
        .unwrap();
    let thread = client
        .thread_with_id("t1")
        .with_interrupts(vec![
            Interrupt::new("i1", "a").with_expires_at("next tuesday"),
            Interrupt::new("i2", "b"),
        ])
        .with_clock(Arc::new(|| at(u32::MAX as u64)));
    thread
        .validate_resume(&[
            ResumeEntry::cancelled("i2"),
            ResumeEntry::resolved("i1", json!(1)),
        ])
        .unwrap();
    let warned = sink.take();
    assert_eq!(warned.len(), 1);
    assert_eq!(warned[0].code, DiagnosticCode::UnparseableExpiry);
    assert_eq!(warned[0].pointer, "/resume/1");
}

/// covers: INTR-15
#[test]
fn an_expired_interrupt_can_still_be_abandoned_and_the_thread_moves_on() {
    let open = vec![Interrupt::new("i1", "a").with_expires_at("2020-01-01T00:00:00Z")];
    let (mut thread, transport) = thread_with(run_of("t1", "r2", vec![]), open);
    let handle = thread
        .run(
            RunRequest::new()
                .run_id("r2")
                .resume(ResumeEntry::cancelled("i1")),
        )
        .unwrap();
    let outcome = block_on(std::future::IntoFuture::into_future(handle));
    thread.absorb(&outcome).unwrap();
    assert!(thread.open_interrupts().is_empty());
    assert_eq!(transport.requests().len(), 1);
}

/// covers: INTR-16
#[test]
fn the_resuming_run_keeps_thread_history_and_state() {
    let mut first = vec![run_started("t1", "r1")];
    first.extend(text("m1", "need approval"));
    first.push(json!({"type": "STATE_SNAPSHOT", "snapshot": {"step": 3}}));
    first.push(json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": "r1",
                      "outcome": {"type": "interrupt", "interrupts": [{"id": "i1", "reason": "approval"}]}}));
    let (mut thread, _) = thread_with(first, vec![]);
    block_on(thread.run_and_absorb(RunRequest::new().run_id("r1").user("go"))).unwrap();
    let snapshot = thread.snapshot();
    let transport = mock(run_of("t1", "r2", text("m2", "done")));
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let mut thread = Thread::restore(client, snapshot).with_clock(Arc::new(|| at(JAN_1)));
    let report = block_on(
        thread.run_and_absorb(
            RunRequest::new()
                .run_id("r2")
                .resume(ResumeEntry::resolved("i1", json!(true))),
        ),
    )
    .unwrap();
    let sent = transport.last_request().unwrap();
    assert_eq!(sent["threadId"], "t1");
    assert_eq!(sent["state"], json!({"step": 3}));
    let texts: Vec<_> = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].clone())
        .collect();
    assert_eq!(texts, vec![json!("user"), json!("assistant")]);
    assert_eq!(report.messages.len(), 3);
    assert_eq!(thread.state(), &json!({"step": 3}));
    assert!(thread.open_interrupts().is_empty());
    assert_eq!(thread.last_run_id().unwrap().as_str(), "r2");
}

/// covers: INTR-24
#[test]
fn a_resume_list_on_a_run_that_continues_nothing_is_refused() {
    let (mut thread, transport) = thread_with(vec![], vec![]);
    let err = thread
        .resume(vec![ResumeEntry::resolved("i1", json!(true))])
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::ResumeCoverage);
    assert!(transport.requests().is_empty());
}

/// A resuming run that fails answers nothing: the interrupts it was
/// resuming stay open, so the next run must still cover them (the most
/// recent interrupted run is still the one being continued), rather than
/// the thread silently continuing past them. One that closes with
/// `RUN_FINISHED` answers them.
///
/// covers: INTR-10, INTR-11, INTR-12
#[test]
fn a_failed_resume_leaves_the_interrupts_open() {
    let resuming = |run: &str| {
        RunRequest::new()
            .run_id(run)
            .resume(ResumeEntry::resolved("i1", json!(true)))
            .resume(ResumeEntry::cancelled("i2"))
    };
    // RUN_ERROR, and a stream cut off inside the run.
    for failure in [
        vec![
            run_started("t1", "r2"),
            json!({"type": "RUN_ERROR", "message": "resume failed"}),
        ],
        vec![run_started("t1", "r2")],
    ] {
        let (mut thread, transport) = thread_with(failure, two_open());
        // Absorbed either way: a report, or an error's partial report.
        let _ = block_on(thread.run_and_absorb(resuming("r2")));
        let open: Vec<&str> = thread
            .open_interrupts()
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(open, ["i1", "i2"]);
        let err = thread
            .run(RunRequest::new().run_id("r3").user("carry on"))
            .expect_err("the open interrupts still need entries");
        assert_eq!(err.code(), ErrorCode::ResumeCoverage);
        assert_eq!(transport.requests().len(), 1, "nothing more was sent");
    }

    // A resume that finishes answers them.
    let (mut thread, _) = thread_with(run_of("t1", "r2", vec![]), two_open());
    block_on(thread.run_and_absorb(resuming("r2"))).unwrap();
    assert!(thread.open_interrupts().is_empty());

    // After a failed resume the producer may start another run on the same
    // stream; one that ends interrupted is the most recent interrupted run.
    let mut frames = vec![
        run_started("t1", "r2"),
        json!({"type": "RUN_ERROR", "message": "resume failed"}),
    ];
    frames.extend(interrupted(
        "r3",
        json!([{"id": "i9", "reason": "approval"}]),
    ));
    let (mut thread, _) = thread_with(frames, two_open());
    block_on(thread.run_and_absorb(resuming("r2"))).unwrap();
    let open: Vec<&str> = thread
        .open_interrupts()
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(open, ["i9"]);
}
