//! State-reuse unit tests. Rule tests live in the `tests/` rule files.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;

use super::*;
use crate::error::ErrorCode;
use crate::ids::{MessageId, RunId, ThreadId};

fn ev(value: serde_json::Value) -> Event {
    serde_json::from_value(value).expect("test event")
}

#[test]
fn run_scoped_tracking_is_cleared_but_keeps_capacity() {
    let mut v = ConsumerVerifier::new();
    v.observe(&ev(
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r1"}),
    ))
    .unwrap();
    for i in 0..64 {
        let id = format!("m{i}");
        v.observe(&ev(json!({"type": "TEXT_MESSAGE_START", "messageId": id})))
            .unwrap();
    }
    v.observe(&ev(json!({"type": "RUN_ERROR", "message": "boom"})))
        .unwrap();
    assert_eq!(v.state.text.open, 0);
    assert!(v.state.text.items.is_empty());
    assert!(v.state.text.items.capacity() >= 64);
    v.observe(&ev(
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2"}),
    ))
    .unwrap();
    v.observe(&ev(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m0"}),
    ))
    .unwrap();
    assert_eq!(v.state.text.open, 1);
}

fn started(run: &str) -> Event {
    ev(json!({"type": "RUN_STARTED", "threadId": "t", "runId": run}))
}

/// pipeline-verify-3: a table inflated by one run is dropped, not cleared at
/// O(capacity), when later runs are small.
#[test]
fn an_oversized_tracker_is_dropped_after_a_small_run() {
    let floor = crate::pipeline::RETAIN_FLOOR;
    let mut v = ConsumerVerifier::new();
    v.observe(&started("r1")).unwrap();
    for i in 0..100_000 {
        let id = format!("m{i}");
        v.observe(&ev(json!({"type": "TEXT_MESSAGE_START", "messageId": id})))
            .unwrap();
    }
    v.observe(&ev(json!({"type": "RUN_ERROR", "message": "boom"})))
        .unwrap();
    v.observe(&started("r2")).unwrap();
    v.observe(&ev(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "only"}),
    ))
    .unwrap();
    v.observe(&ev(
        json!({"type": "TEXT_MESSAGE_END", "messageId": "only"}),
    ))
    .unwrap();
    v.observe(&ev(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r2"}),
    ))
    .unwrap();
    assert!(
        v.state.text.items.capacity() <= floor,
        "capacity {}",
        v.state.text.items.capacity()
    );
}

/// pipeline-verify-3: the per-step owner sets and the activity table too.
#[test]
fn oversized_owner_sets_and_activity_are_dropped_after_a_small_run() {
    let floor = crate::pipeline::RETAIN_FLOOR;
    let mut v = ConsumerVerifier::new();
    v.observe(&started("r1")).unwrap();
    for i in 0..50_000 {
        let owner = format!("o{i}");
        v.observe(&ev(
            json!({"type": "STEP_STARTED", "stepName": "s", "subagentRunId": owner}),
        ))
        .unwrap();
    }
    for i in 0..100_000 {
        let id = format!("a{i}");
        v.observe(&ev(
            json!({"type": "ACTIVITY_SNAPSHOT", "messageId": id, "activityType": "x", "content": {}}),
        ))
        .unwrap();
    }
    v.observe(&ev(json!({"type": "RUN_ERROR", "message": "boom"})))
        .unwrap();
    v.observe(&started("r2")).unwrap();
    v.observe(&ev(json!({"type": "STEP_STARTED", "stepName": "s"})))
        .unwrap();
    v.observe(&ev(json!({"type": "STEP_FINISHED", "stepName": "s"})))
        .unwrap();
    v.observe(&ev(
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "x", "content": {}}),
    ))
    .unwrap();
    v.observe(&ev(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r2"}),
    ))
    .unwrap();
    assert!(v.state.steps["s"].capacity() <= floor);
    assert!(v.state.activity.capacity() <= floor);
}

/// pipeline-verify-4: step names do not accumulate within a run.
#[test]
fn distinct_finished_step_names_are_not_retained_within_a_run() {
    let mut v = ConsumerVerifier::new();
    v.observe(&started("r1")).unwrap();
    for i in 0..10_000 {
        let name = format!("s{i}");
        v.observe(&ev(json!({"type": "STEP_STARTED", "stepName": name})))
            .unwrap();
        v.observe(&ev(json!({"type": "STEP_FINISHED", "stepName": name})))
            .unwrap();
    }
    assert!(
        v.state.steps.len() <= MAX_RETAINED_STEP_NAMES + 1,
        "{} names kept",
        v.state.steps.len()
    );
    // A name that was dropped can start and finish again.
    v.observe(&ev(json!({"type": "STEP_STARTED", "stepName": "s0"})))
        .unwrap();
    v.observe(&ev(json!({"type": "STEP_FINISHED", "stepName": "s0"})))
        .unwrap();
    assert!(v
        .observe(&ev(json!({"type": "STEP_FINISHED", "stepName": "s0"})))
        .is_err());
    v.observe(&ev(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}),
    ))
    .unwrap();
}

/// pipeline-verify-6: peer ids in error text are previewed and counted, not
/// interpolated at full length.
#[test]
fn error_text_bounds_peer_ids() {
    let mut v = ConsumerVerifier::new();
    v.observe(&started("r1")).unwrap();
    for i in 0..1_000 {
        let id = format!("{i:04}{}", "x".repeat(10 * 1024));
        v.observe(&ev(json!({"type": "TEXT_MESSAGE_START", "messageId": id})))
            .unwrap();
    }
    let err = v
        .observe(&ev(
            json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}),
        ))
        .unwrap_err();
    let text = err.message().to_owned();
    assert!(text.len() < 4096, "{} bytes", text.len());
    assert!(text.contains(", and 984 more"), "{}", &text[..200]);

    // Control characters in a peer id cannot reach the text raw.
    let mut v2 = ConsumerVerifier::new();
    v2.observe(&started("r1")).unwrap();
    v2.observe(&ev(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "a\n[ERROR] forged\u{1b}[2J"}),
    ))
    .unwrap();
    let err = v2
        .observe(&ev(
            json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}),
        ))
        .unwrap_err();
    let text = err.message();
    assert!(text.contains("forged"), "{text}");
    assert!(!text.contains('\n') && !text.contains('\u{1b}'), "{text:?}");

    let big = "y".repeat(1 << 20);
    let err = v
        .observe(&ev(
            json!({"type": "TOOL_CALL_RESULT", "messageId": format!("0000{}", "x".repeat(10 * 1024)),
                   "toolCallId": big, "content": "c"}),
        ))
        .unwrap_err();
    assert!(err.message().len() < 1024, "{} bytes", err.message().len());
    let err = v
        .observe(&ev(json!({"type": "STEP_FINISHED", "stepName": big})))
        .unwrap_err();
    assert!(err.message().len() < 1024, "{} bytes", err.message().len());
}

#[test]
fn a_refused_event_leaves_state_unchanged() {
    let mut v = ConsumerVerifier::new();
    v.observe(&ev(
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r1"}),
    ))
    .unwrap();
    v.observe(&ev(json!({"type": "TEXT_MESSAGE_START", "messageId": "m"})))
        .unwrap();
    assert!(v
        .observe(&ev(
            json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"})
        ))
        .is_err());
    assert_eq!(v.phase(), RunPhase::Running);
    assert_eq!(v.state.text.open, 1);
}

fn rs(thread: &str, run: &str) -> Event {
    ev(json!({"type": "RUN_STARTED", "threadId": thread, "runId": run}))
}

fn rf(thread: &str, run: &str) -> Event {
    ev(json!({"type": "RUN_FINISHED", "threadId": thread, "runId": run}))
}

#[test]
fn expecting_accepts_replayed_runs_before_the_requested_one() {
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r3"));
    for run in ["r1", "r2", "r3"] {
        v.observe(&rs("t", run)).unwrap();
        v.observe(&rf("t", run)).unwrap();
    }
    assert_eq!(v.phase(), RunPhase::Finished);
}

/// covers: RUNIN-04
#[test]
fn expecting_refuses_another_thread_on_either_boundary() {
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r1"));
    let err = v.observe(&rs("other", "r1")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Identity);
    assert_eq!(v.phase(), RunPhase::Idle);

    // A later run on another thread is refused too, not only the first.
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r1"));
    v.observe(&rs("t", "r1")).unwrap();
    v.observe(&rf("t", "r1")).unwrap();
    let err = v.observe(&rs("other", "r2")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Identity);

    // RUN_FINISHED naming another thread than its run's.
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r1"));
    v.observe(&rs("t", "r1")).unwrap();
    let err = v.observe(&rf("other", "r1")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Identity);
    assert_eq!(v.phase(), RunPhase::Running);

    // Without expectations, any sequence of runs is fine.
    let mut v = ConsumerVerifier::new();
    for (thread, run) in [("t", "r1"), ("u", "r2")] {
        v.observe(&rs(thread, run)).unwrap();
        v.observe(&rf(thread, run)).unwrap();
    }
    v.finish().unwrap();
}

/// covers: RUNIN-05
#[test]
fn each_run_boundary_pair_agrees_on_run_id() {
    for verifier in [
        ConsumerVerifier::new(),
        ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r1")),
    ] {
        let mut v = verifier;
        v.observe(&rs("t", "r1")).unwrap();
        let err = v.observe(&rf("t", "r2")).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Identity);
        assert!(err.message().contains("'r2'"), "{}", err.message());
    }
    // The second run of a stream is held to its own id, not the first's.
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r1"));
    v.observe(&rs("t", "r1")).unwrap();
    v.observe(&rf("t", "r1")).unwrap();
    v.observe(&rs("t", "r2")).unwrap();
    assert_eq!(
        v.observe(&rf("t", "r1")).unwrap_err().code(),
        ErrorCode::Identity
    );
    v.observe(&rf("t", "r2")).unwrap();
}

/// covers: RUNIN-06
#[test]
fn the_requested_run_may_come_anywhere_but_must_appear() {
    let error = || ev(json!({"type": "RUN_ERROR", "message": "boom"}));
    // Requested first, then a run the agent started after it.
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r1"));
    v.observe(&rs("t", "r1")).unwrap();
    v.observe(&error()).unwrap();
    v.observe(&rs("t", "r2")).unwrap();
    v.observe(&rf("t", "r2")).unwrap();
    v.finish().unwrap();

    // Requested in the middle.
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r2"));
    for run in ["r1", "r2", "r3"] {
        v.observe(&rs("t", run)).unwrap();
        v.observe(&rf("t", run)).unwrap();
    }
    v.finish().unwrap();

    // Never there: every run named another id.
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r9"));
    for run in ["r1", "r2"] {
        v.observe(&rs("t", run)).unwrap();
        v.observe(&rf("t", run)).unwrap();
    }
    let err = v.finish().unwrap_err();
    assert_eq!(err.code(), ErrorCode::Identity);
    assert!(err.message().contains("'r9'"), "{}", err.message());

    // Same, ending on a failed run.
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r9"));
    v.observe(&rs("t", "r1")).unwrap();
    v.observe(&error()).unwrap();
    assert_eq!(v.finish().unwrap_err().code(), ErrorCode::Identity);

    // A stream that opens with RUN_ERROR is the requested run failing
    // before it started (LIFE-03); it carries no ids to compare.
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r9"));
    v.observe(&error()).unwrap();
    v.finish().unwrap();
    // A late RUN_ERROR after another run does not stand in for it.
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r9"));
    v.observe(&rs("t", "r1")).unwrap();
    v.observe(&rf("t", "r1")).unwrap();
    v.observe(&error()).unwrap();
    assert_eq!(v.finish().unwrap_err().code(), ErrorCode::Identity);

    // A stream that ends inside a run is truncated, not misidentified.
    let mut v = ConsumerVerifier::expecting(ThreadId::new("t"), RunId::new("r9"));
    v.observe(&rs("t", "r1")).unwrap();
    v.finish().unwrap();
    assert_eq!(v.phase(), RunPhase::Running);
}

#[test]
fn known_messages_cannot_be_reminted_but_may_be_reopened() {
    let mut v = ConsumerVerifier::new().with_known_messages([MessageId::new("held")]);
    v.observe(&rs("t", "r1")).unwrap();
    // Restreaming a held message appends (LIFE-47).
    v.observe(&ev(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "held"}),
    ))
    .unwrap();
    v.observe(&ev(
        json!({"type": "TEXT_MESSAGE_END", "messageId": "held"}),
    ))
    .unwrap();
    let result = |id: &str| {
        ev(json!({"type": "TOOL_CALL_RESULT", "messageId": id, "toolCallId": "c", "content": "x"}))
    };
    let err = v.observe(&result("held")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Identity);
    // Ids the stream itself minted count too.
    v.observe(&ev(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "new"}),
    ))
    .unwrap();
    assert!(v.observe(&result("new")).is_err());
    v.observe(&result("fresh")).unwrap();
    assert!(v.observe(&result("fresh")).is_err());
}

/// The remembered delta target never lets a continuation through that the
/// table would refuse: after its item closed, under another owner, or in a
/// later run.
#[test]
fn a_remembered_delta_target_is_forgotten_when_its_item_changes() {
    let content = |owner: Option<&str>| {
        let mut e = json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "x"});
        if let Some(owner) = owner {
            e["subagentRunId"] = json!(owner);
        }
        ev(e)
    };
    let mut v = ConsumerVerifier::new();
    v.observe(&started("r1")).unwrap();
    v.observe(&ev(
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s", "name": "n"}),
    ))
    .ok();
    v.observe(&ev(json!({"type": "TEXT_MESSAGE_START", "messageId": "m"})))
        .unwrap();
    v.observe(&content(None)).unwrap();
    v.observe(&content(None)).unwrap();
    assert!(v.state.text.hot.is_some(), "a repeated delta is remembered");
    // Another owner's continuation is refused although `m` is remembered.
    assert!(v.observe(&content(Some("s"))).is_err());
    v.observe(&content(None)).unwrap();
    v.observe(&ev(json!({"type": "TEXT_MESSAGE_END", "messageId": "m"})))
        .unwrap();
    assert!(v.state.text.hot.is_none(), "a close forgets it");
    assert!(v.observe(&content(None)).is_err(), "closed: refused");
    // Reopened, remembered, then the run ends: the next run does not know `m`.
    v.observe(&ev(json!({"type": "TEXT_MESSAGE_START", "messageId": "m"})))
        .unwrap();
    v.observe(&content(None)).unwrap();
    v.observe(&ev(json!({"type": "RUN_ERROR", "message": "boom"})))
        .unwrap();
    assert!(v.state.text.hot.is_none(), "a reset forgets it");
    v.observe(&started("r2")).unwrap();
    assert!(v.observe(&content(None)).is_err(), "new run: refused");
}

/// subagents.mdx: a tool call inherits the attribution of the message
/// holding it, so a continuation tagged with that owner agrees, whether the
/// message was restated by a snapshot, held by the history or streamed by an
/// earlier run.
#[test]
fn an_untagged_call_inherits_a_held_parent_owner() {
    let call = |id: &str, parent: &str| {
        ev(
            json!({"type": "TOOL_CALL_START", "toolCallId": id, "toolCallName": "x",
                  "parentMessageId": parent}),
        )
    };
    let args = |id: &str, sub: &str| {
        ev(
            json!({"type": "TOOL_CALL_ARGS", "toolCallId": id, "delta": "{}",
                  "subagentRunId": sub}),
        )
    };

    // Restated by a snapshot of this run: the owner is known.
    let mut v = ConsumerVerifier::new();
    v.observe(&started("r1")).unwrap();
    v.observe(&ev(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "m1", "role": "assistant", "content": "x", "subagentRunId": "A"}
    ]})))
    .unwrap();
    v.observe(&call("c1", "m1")).unwrap();
    let err = v.observe(&args("c1", "B")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Ownership);
    v.observe(&args("c1", "A")).unwrap();

    // Held by the history: the first tagged continuation names the owner.
    let mut v = ConsumerVerifier::new().with_known_messages([MessageId::from("h1")]);
    v.observe(&started("r1")).unwrap();
    v.observe(&call("c1", "h1")).unwrap();
    v.observe(&args("c1", "A")).unwrap();
    let err = v.observe(&args("c1", "B")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Ownership);

    // Streamed by an earlier run.
    let mut v = ConsumerVerifier::new();
    v.observe(&started("r1")).unwrap();
    v.observe(&ev(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "A"}),
    ))
    .unwrap();
    v.observe(&ev(
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "subagentRunId": "A"}),
    ))
    .unwrap();
    v.observe(&ev(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}),
    ))
    .unwrap();
    v.observe(&started("r2")).unwrap();
    v.observe(&call("c1", "m1")).unwrap();
    v.observe(&args("c1", "A")).unwrap();
}

/// D25: a run-scoped table's charge is credited when the run ends, so the
/// table itself does not outlive the run at a capacity beyond the floor.
#[test]
fn a_large_run_leaves_no_uncharged_table_behind() {
    let floor = crate::pipeline::RETAIN_FLOOR;
    let mut v = ConsumerVerifier::new();
    v.observe(&started("r1")).unwrap();
    for i in 0..100_000 {
        let id = format!("s{i}");
        v.observe(&ev(json!({"type": "REASONING_START", "messageId": id})))
            .unwrap();
        v.observe(&ev(json!({"type": "REASONING_END", "messageId": id})))
            .unwrap();
    }
    for i in 0..5_000 {
        let owner = format!("o{i}");
        v.observe(&ev(
            json!({"type": "STEP_STARTED", "stepName": "s", "subagentRunId": owner}),
        ))
        .unwrap();
        v.observe(&ev(
            json!({"type": "STEP_FINISHED", "stepName": "s", "subagentRunId": owner}),
        ))
        .unwrap();
    }
    v.observe(&ev(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}),
    ))
    .unwrap();
    assert_eq!(v.retained(), v.state.step_name_bytes);
    let spans = v.state.spans.items.capacity();
    assert!(spans <= floor, "spans {spans}");
    let owners = v.state.steps.get("s").map_or(0, HashSet::capacity);
    assert!(owners <= floor, "step owners {owners}");
}
