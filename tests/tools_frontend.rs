//! Frontend tools, consumer side (`events/tool-calls.mdx` "Frontend tools"):
//! pending calls are derived from the stream, and a thread that continues
//! answers every one of them first (a call an open interrupt names is
//! answered by that interrupt's resume entry, D13).

#![cfg(feature = "client")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_client;

use agui_sdk::client::{AgentClient, DecidedBy, RunRequest, Thread, ThreadSnapshot};
use agui_sdk::content::{ContentPart, MediaPart, PartSource, UrlSource};
use agui_sdk::diagnostics::NullSink;
use agui_sdk::ids::ToolCallId;
use agui_sdk::interrupt::ResumeEntry;
use agui_sdk::{ErrorCode, Message};
use serde_json::{json, Value};
use support_client::*;

fn user() -> DecidedBy {
    DecidedBy::User { user_id: None }
}

/// A run that leaves `calls` unanswered and finishes with `outcome`.
fn calling(run: &str, calls: &[&str], outcome: Option<Value>) -> Vec<Value> {
    let mut frames = vec![run_started("t1", run)];
    frames.extend(text("m1", "let me check"));
    for id in calls {
        frames.extend(tool_call(id, "pick_file", r#"{"kind":"image"}"#, "m1"));
    }
    let mut finished = run_finished("t1", run);
    if let Some(outcome) = outcome {
        finished["outcome"] = outcome;
    }
    frames.push(finished);
    frames
}

fn thread(frames: Vec<Value>) -> (Thread<MockTransport>, MockTransport) {
    let transport = mock(frames);
    let client = AgentClient::with_transport(transport.clone())
        .diagnostic_sink(NullSink)
        .build()
        .unwrap();
    (client.thread_with_id("t1"), transport)
}

/// The thread after a run that left `calls` pending; its next run is
/// served `next`.
fn pending_thread(calls: &[&str], next: Vec<Value>) -> (Thread<MockTransport>, MockTransport) {
    let (mut first, _) = thread(calling("r1", calls, None));
    block_on(first.run_and_absorb(RunRequest::new().run_id("r1").user("pick one"))).unwrap();
    let transport = mock(next);
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    (Thread::restore(client, first.snapshot()), transport)
}

/// covers: TOOL-32
#[test]
fn pending_calls_are_derived_from_the_stream_not_from_the_outcome() {
    // No pendingToolCallIds at all: absence is not "nothing pending".
    let (mut t, _) = thread(calling("r1", &["tc1", "tc2"], None));
    let report = block_on(t.run_and_absorb(RunRequest::new().run_id("r1"))).unwrap();
    let ids: Vec<_> = t
        .pending_tool_calls()
        .iter()
        .map(|c| c.tool_call_id.as_str())
        .collect();
    assert_eq!(ids, ["tc1", "tc2"]);
    assert_eq!(report.pending_tool_calls.len(), 2);
    assert_eq!(t.pending_tool_calls()[0].arguments, r#"{"kind":"image"}"#);
    assert_eq!(t.pending_tool_calls()[0].message_id.as_str(), "m1");

    // An empty list is not "nothing pending" either.
    let outcome = json!({"type": "success", "pendingToolCallIds": []});
    let (mut t, _) = thread(calling("r1", &["tc1"], Some(outcome)));
    block_on(t.run_and_absorb(RunRequest::new().run_id("r1"))).unwrap();
    assert_eq!(t.pending_tool_calls().len(), 1);

    // On an interrupted run the calls are derived the same way.
    let outcome = json!({"type": "interrupt", "interrupts": [{"id": "i1", "reason": "r"}]});
    let (mut t, _) = thread(calling("r1", &["tc1"], Some(outcome)));
    block_on(t.run_and_absorb(RunRequest::new().run_id("r1"))).unwrap();
    assert_eq!(t.pending_tool_calls().len(), 1);
    assert_eq!(t.open_interrupts().len(), 1);

    // A call the stream itself answered is not pending.
    let mut frames = calling("r1", &["tc1", "tc2"], None);
    let at = frames.len() - 1;
    frames.insert(
        at,
        json!({"type": "TOOL_CALL_RESULT", "messageId": "tr1", "toolCallId": "tc1", "content": "ok"}),
    );
    let (mut t, _) = thread(frames);
    let report = block_on(t.run_and_absorb(RunRequest::new().run_id("r1"))).unwrap();
    let ids: Vec<_> = t
        .pending_tool_calls()
        .iter()
        .map(|c| c.tool_call_id.as_str())
        .collect();
    assert_eq!(ids, ["tc2"]);
    assert_eq!(report.pending_tool_calls.len(), 1);
}

/// covers: TOOL-33
#[test]
fn continuing_with_an_unanswered_call_is_refused_before_sending() {
    let (mut t, transport) = pending_thread(&["tc1", "tc2"], run_of("t1", "r2", vec![]));
    let call = t.approve(&ToolCallId::from("tc1"), user()).unwrap();
    t.complete(call, "file.png").unwrap();
    let err = t.send(Message::user("next")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::PendingToolCalls);
    assert!(err.message().starts_with("1 frontend"), "{}", err.message());
    assert!(transport.requests().is_empty());
}

/// The interrupt-resume.mdx Message Flow: the agent proposes `transfer_funds`
/// (tc1) and stops to ask about it; `extra` adds frontend calls no interrupt
/// names. Returns the thread restored after r1, its next run served `next`.
fn approval_thread(extra: &[&str], next: Vec<Value>) -> (Thread<MockTransport>, MockTransport) {
    let mut r1 = vec![run_started("t1", "r1")];
    r1.extend(text("m1", "about to transfer"));
    r1.extend(tool_call("tc1", "transfer_funds", r#"{"amount":5}"#, "m1"));
    for id in extra {
        r1.extend(tool_call(id, "pick_file", "{}", "m1"));
    }
    let mut finished = run_finished("t1", "r1");
    finished["outcome"] = json!({"type": "interrupt",
        "interrupts": [{"id": "int-1", "reason": "approval", "toolCallId": "tc1"}]});
    r1.push(finished);
    let (mut first, _) = thread(r1);
    block_on(first.run_and_absorb(RunRequest::new().run_id("r1").user("pay"))).unwrap();
    // Derived the same way on an interrupted run; the call stays listed.
    let mut expected = vec!["tc1"];
    expected.extend_from_slice(extra);
    let ids: Vec<_> = first
        .pending_tool_calls()
        .iter()
        .map(|c| c.tool_call_id.as_str())
        .collect();
    assert_eq!(ids, expected);
    let transport = mock(next);
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    (Thread::restore(client, first.snapshot()), transport)
}

/// covers: TOOL-33
#[test]
fn a_call_an_approval_interrupt_names_is_answered_by_the_resume_entry() {
    let (mut t, transport) = approval_thread(&["tc2"], run_of("t1", "r2", vec![]));

    // tc2 still needs an answer: the exemption is only for the named call.
    let err = t
        .resume(vec![ResumeEntry::resolved("int-1", json!(true))])
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::PendingToolCalls);
    assert!(err.message().starts_with("1 frontend"), "{}", err.message());
    assert!(transport.requests().is_empty());

    // Answer tc2; tc1 is answered by the resume entry alone.
    t.decline(&ToolCallId::from("tc2"), "not now").unwrap();
    let request = RunRequest::new()
        .run_id("r2")
        .resume(ResumeEntry::resolved("int-1", json!(true)));
    block_on(t.run_and_absorb(request)).unwrap();
    let sent = transport.last_request().unwrap();
    assert_eq!(sent["resume"][0]["interruptId"], "int-1");
    let answers: Vec<&Value> = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(answers.len(), 1, "no fabricated result for tc1: {sent}");
    assert_eq!(answers[0]["toolCallId"], "tc2");
    assert!(t.open_interrupts().is_empty());
    assert!(t.pending_tool_calls().is_empty());
}

/// covers: TOOL-33
#[test]
fn a_cancelled_resume_entry_also_answers_the_call_its_interrupt_names() {
    let (mut t, transport) = approval_thread(&[], run_of("t1", "r2", vec![]));
    let request = RunRequest::new()
        .run_id("r2")
        .resume(ResumeEntry::cancelled("int-1"));
    block_on(t.run_and_absorb(request)).unwrap();
    let sent = transport.last_request().unwrap();
    assert_eq!(sent["resume"][0]["interruptId"], "int-1");
    assert!(
        sent["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["role"] != "tool"),
        "{sent}"
    );
    assert!(t.open_interrupts().is_empty());
    assert!(t.pending_tool_calls().is_empty());
}

/// History alone cannot show that a resume entry answered a call (D13):
/// `with_history` lists tc1 again and the thread asks for an answer, while
/// `restore` from a snapshot keeps the exact pending set.
/// covers: TOOL-33
#[test]
fn a_call_answered_by_a_resume_entry_is_listed_again_from_history_alone() {
    let (mut t, _) = approval_thread(&[], run_of("t1", "r2", vec![]));
    let request = RunRequest::new()
        .run_id("r2")
        .resume(ResumeEntry::cancelled("int-1"));
    block_on(t.run_and_absorb(request)).unwrap();
    assert!(t.pending_tool_calls().is_empty());
    let snapshot = t.snapshot();

    // From the snapshot: nothing is pending, the thread continues.
    let transport = mock(run_of("t1", "r3", vec![]));
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let mut restored = Thread::restore(client, snapshot.clone());
    assert!(restored.pending_tool_calls().is_empty());
    block_on(restored.run_and_absorb(RunRequest::new().run_id("r3").user("next"))).unwrap();
    assert_eq!(transport.requests().len(), 1);

    // From history alone: tc1 is listed again and must be answered.
    let transport = mock(run_of("t1", "r3", vec![]));
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let mut rebuilt = Thread::new(client, "t1").with_history(snapshot.messages, snapshot.state);
    let ids: Vec<_> = rebuilt
        .pending_tool_calls()
        .iter()
        .map(|c| c.tool_call_id.as_str())
        .collect();
    assert_eq!(ids, ["tc1"]);
    let err = rebuilt.send(Message::user("next")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::PendingToolCalls);
    assert!(transport.requests().is_empty());
    rebuilt
        .decline(&ToolCallId::from("tc1"), "already cancelled")
        .unwrap();
    block_on(rebuilt.run_and_absorb(RunRequest::new().run_id("r3").user("next"))).unwrap();
    assert_eq!(transport.requests().len(), 1);
}

/// covers: TOOL-33
#[test]
fn every_call_is_answered_by_a_tool_message_keyed_by_its_id() {
    let (mut t, transport) = pending_thread(&["tc1", "tc2", "tc3"], run_of("t1", "r2", vec![]));
    let call = t.approve(&ToolCallId::from("tc1"), user()).unwrap();
    t.complete(call, "chosen.png").unwrap();
    let call = t.approve(&ToolCallId::from("tc2"), user()).unwrap();
    t.fail(call, "picker crashed").unwrap();
    t.decline(&ToolCallId::from("tc3"), "the user declined")
        .unwrap();
    assert!(t.pending_tool_calls().is_empty());
    block_on(t.run_and_absorb(RunRequest::new().run_id("r2").user("continue"))).unwrap();

    let sent = transport.last_request().unwrap();
    let tools: Vec<&Value> = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0]["toolCallId"], "tc1");
    assert_eq!(tools[0]["content"], "chosen.png");
    assert!(tools[0].get("error").is_none());
    assert_eq!(tools[1]["toolCallId"], "tc2");
    assert_eq!(tools[1]["error"], "picker crashed");
    assert_eq!(tools[2]["toolCallId"], "tc3");
    assert_eq!(tools[2]["content"], "the user declined");
    // Answers precede the new user message, after the history (the
    // assistant message that made the calls).
    let msgs = sent["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs
        .iter()
        .map(|m| m["role"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "tool", "tool", "tool", "user"],
        "{sent}"
    );
    let call_ids: Vec<&str> = msgs[1]["toolCalls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(call_ids, ["tc1", "tc2", "tc3"]);
    // The answers are now history; nothing is staged twice.
    assert_eq!(t.snapshot().staged.len(), 0);
}

/// covers: TOOL-33
#[test]
fn an_answer_in_the_request_itself_also_counts() {
    let (mut t, transport) = pending_thread(&["tc1"], run_of("t1", "r2", vec![]));
    let answer = Message::tool("tc1", "done by hand");
    block_on(t.run_and_absorb(RunRequest::new().run_id("r2").message(answer))).unwrap();
    assert_eq!(transport.requests().len(), 1);
}

/// covers: TOOL-33
#[test]
fn pending_calls_survive_a_restart_through_a_snapshot() {
    let (first, _) = pending_thread(&["tc1"], vec![]);
    let saved = serde_json::to_string(&first.snapshot()).unwrap();
    let restored: ThreadSnapshot = serde_json::from_str(&saved).unwrap();
    assert_eq!(restored, first.snapshot());
    let transport = mock(vec![]);
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let mut t = Thread::restore(client, restored);
    assert_eq!(t.pending_tool_calls().len(), 1);
    assert_eq!(
        t.send(Message::user("x")).unwrap_err().code(),
        ErrorCode::PendingToolCalls
    );

    // History alone also brings them back (derived, TOOL-32).
    let client = AgentClient::with_transport(transport).build().unwrap();
    let t = client
        .thread_with_id("t1")
        .with_history(first.messages().to_vec(), json!({}));
    assert_eq!(t.pending_tool_calls().len(), 1);
}

/// covers: TOOL-33
#[test]
fn a_call_can_be_answered_only_once_and_only_if_pending() {
    let (mut t, _) = pending_thread(&["tc1"], vec![]);
    let call = t.approve(&ToolCallId::from("tc1"), user()).unwrap();
    let again = call.clone();
    t.complete(call, "ok").unwrap();
    assert_eq!(
        t.complete(again, "twice").unwrap_err().code(),
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        t.decline(&ToolCallId::from("tc1"), "no")
            .unwrap_err()
            .code(),
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        t.approve(&ToolCallId::from("nope"), user())
            .unwrap_err()
            .code(),
        ErrorCode::InvalidRequest
    );
}

/// covers: TOOL-34
#[test]
fn an_answer_may_be_media_parts_and_abandoning_the_thread_is_fine() {
    let (mut t, transport) = pending_thread(&["tc1"], run_of("t1", "r2", vec![]));
    let call = t.approve(&ToolCallId::from("tc1"), user()).unwrap();
    let part = ContentPart::Image(MediaPart::new(PartSource::Url(
        UrlSource::new("https://example.com/shot.png").with_mime_type("image/png"),
    )));
    t.complete(call, vec![part]).unwrap();
    block_on(t.run_and_absorb(RunRequest::new().run_id("r2"))).unwrap();
    let sent = transport.last_request().unwrap();
    let answer = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    assert_eq!(answer["content"][0]["type"], "image");
    assert_eq!(
        answer["content"][0]["source"]["value"],
        "https://example.com/shot.png"
    );

    // Abandoning a thread with calls pending answers nothing and violates
    // nothing: dropping it is not an error.
    let (abandoned, _) = pending_thread(&["tc9"], vec![]);
    drop(abandoned);
}

/// covers: TOOL-33
#[test]
fn staged_answers_survive_a_run_that_delivered_nothing() {
    let (mut t, _) = pending_thread(&["tc1"], vec![]);
    let call = t.approve(&ToolCallId::from("tc1"), user()).unwrap();
    t.complete(call, "ok").unwrap();
    // This client's transport refuses the request: nothing was delivered.
    let refusing = mock(vec![]).refusing(ErrorCode::Connect, "down");
    let client = AgentClient::with_transport(refusing).build().unwrap();
    let mut t = Thread::restore(client, t.snapshot());
    let err = block_on(t.run_and_absorb(RunRequest::new().run_id("r2"))).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Connect);
    // The answer is still staged for the retry, and the thread is free again.
    assert_eq!(t.snapshot().staged.len(), 1);
    assert!(t.run(RunRequest::new().run_id("r3")).is_ok());
}

/// covers: TOOL-33
#[test]
fn absorbing_another_runs_outcome_is_refused() {
    let (mut t, _) = thread(calling("r1", &["tc1"], None));
    let other = {
        let (mut o, _) = thread(calling("r1", &["tc1"], None));
        block_on(std::future::IntoFuture::into_future(
            o.run(RunRequest::new().run_id("r1")).unwrap(),
        ))
    };
    // Nothing in flight yet.
    assert_eq!(
        t.absorb(&other).unwrap_err().code(),
        ErrorCode::InvalidRequest
    );
    let handle = t.run(RunRequest::new().run_id("r-mine")).unwrap();
    assert_eq!(
        t.absorb(&other).unwrap_err().code(),
        ErrorCode::InvalidRequest
    );
    drop(handle);
}

/// covers: TOOL-33
#[test]
fn answering_the_middle_call_leaves_the_others_pending_and_it_unanswerable() {
    let (mut t, _) = pending_thread(&["a", "b", "c"], vec![]);
    let b = ToolCallId::from("b");
    let call = t.approve(&b, user()).unwrap();
    t.complete(call, "ok").unwrap();
    let mut left: Vec<_> = t
        .pending_tool_calls()
        .iter()
        .map(|c| c.tool_call_id.as_str().to_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["a", "c"]);
    assert_eq!(
        t.approve(&b, user()).unwrap_err().code(),
        ErrorCode::InvalidRequest
    );
    // The calls that moved are still found and answerable.
    for id in ["c", "a"] {
        let id = ToolCallId::from(id);
        t.decline(&id, "no").unwrap();
    }
    assert!(t.pending_tool_calls().is_empty());
    assert_eq!(t.snapshot().staged.len(), 3);
}
