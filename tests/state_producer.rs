//! Server-side state: the publisher's baseline, deltas versus snapshots,
//! resynchronisation, and message snapshots.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_server;

use agui_sdk::event::Event;
use agui_sdk::patch::{self, JsonPointer, PatchBudget, PatchOperation};
use agui_sdk::server::state::StatePublisher;
use agui_sdk::server::{CancelToken, Extensions, ReplayedRun, RunOutcome, Runner, Transformer};
use agui_sdk::state::{MessagesSnapshot, StateDelta, StateSnapshot};
use agui_sdk::{ErrorCode, EventType, Message};
use serde_json::{json, Value};

use support_server::*;

/// Applies state events the way a client does.
fn apply_all(mut client: Value, events: &[Event]) -> Value {
    for event in events {
        match event {
            Event::StateSnapshot(s) => client = s.snapshot.clone(),
            Event::StateDelta(d) => {
                let mut budget = PatchBudget::for_document(&client, Default::default());
                patch::apply(&mut client, &d.delta, &mut budget)
                    .unwrap_or_else(|e| panic!("delta did not apply: {e:?}"));
            }
            _ => {}
        }
    }
    client
}

/// covers: SNAP-03, SNAP-15
#[test]
fn a_snapshot_can_be_sent_at_any_time() {
    let runner = Runner::new(agent(|mut ctx| async move {
        ctx.emitter().say("before").await?;
        *ctx.state_mut() = json!({"n": 1});
        ctx.resync_state().await?;
        ctx.resync_state().await?; // even when nothing changed
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input().with_state(json!({"n": 1})));
    let snapshots = types(&events)
        .into_iter()
        .filter(|t| *t == EventType::StateSnapshot)
        .count();
    assert_eq!(snapshots, 2);
}

/// covers: SNAP-04
#[test]
fn a_baseline_that_cannot_be_vouched_for_gets_a_snapshot() {
    let pad = "p".repeat(200);
    let mut publisher = StatePublisher::new(json!({"a": 1, "pad": pad}));
    // A delta the client could not have applied: the baseline is now unknown.
    let bad = StateDelta::new(vec![PatchOperation::remove(JsonPointer::new_unchecked(
        "/missing",
    ))]);
    publisher.observe(&bad.into());
    let next = json!({"a": 2, "pad": pad});
    let event = publisher.publish(&next).unwrap();
    assert_eq!(event.event_type(), EventType::StateSnapshot);
    // Once resynchronised, deltas resume.
    let event = publisher.publish(&json!({"a": 3, "pad": pad})).unwrap();
    assert_eq!(event.event_type(), EventType::StateDelta);
}

/// covers: SNAP-05
#[test]
fn published_deltas_apply_in_order_to_the_clients_copy() {
    let mut publisher = StatePublisher::new(json!({"list": [1, 2], "keep": true, "gone": 0}));
    let mut client = json!({"list": [1, 2], "keep": true, "gone": 0});
    let steps = [
        json!({"list": [1, 2, 3], "keep": true, "gone": 0}),
        json!({"list": [1, 2, 3], "keep": false, "new": {"x": 1}}),
        json!({"list": [], "keep": false, "new": {"x": 2}}),
    ];
    for next in steps {
        if let Some(event) = publisher.publish(&next) {
            client = apply_all(client, &[event]);
        }
        assert_eq!(client, next);
    }
    assert!(
        publisher.publish(&client).is_none(),
        "no event when nothing changed"
    );
}

/// covers: SNAP-07
#[test]
fn no_delta_without_a_baseline() {
    // The baseline before the agent's first snapshot is the input state; an
    // absent input state is `{}`, so a delta is always against something the
    // client holds.
    let mut publisher = StatePublisher::new(Value::Null);
    let long = "v".repeat(100);
    // A client holding `{}` can take a delta adding a member, so the
    // publisher's baseline must be `{}` too: after observing one, it keeps
    // sending deltas rather than falling back to a snapshot.
    let add = StateDelta::new(vec![PatchOperation::add(
        JsonPointer::new_unchecked("/k"),
        json!(long),
    )]);
    publisher.observe(&add.into());
    let event = publisher.publish(&json!({"k": long, "n": 1})).unwrap();
    assert_eq!(event.event_type(), EventType::StateDelta);
    let applied = apply_all(json!({"k": long}), &[event]);
    assert_eq!(applied, json!({"k": long, "n": 1}));

    // An activity delta before its snapshot is refused at the call site.
    let runner = Runner::new(agent(|ctx| async move {
        let refused = ctx
            .emitter()
            .activity_delta("a1".into(), "progress", vec![])
            .await;
        assert_eq!(
            refused.err().map(|e| e.code()),
            Some(ErrorCode::ProducerViolation)
        );
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_eq!(
        run_error(&events).code.as_deref(),
        Some("server.producer_violation")
    );
}

#[test]
fn the_smaller_of_delta_and_snapshot_is_sent() {
    let long = "x".repeat(200);
    let mut publisher = StatePublisher::new(json!({"doc": long, "c": 3}));
    let small = publisher.publish(&json!({"doc": long, "c": 4})).unwrap();
    assert_eq!(small.event_type(), EventType::StateDelta);
    let whole = publisher.publish(&json!({"z": 0})).unwrap();
    assert_eq!(whole.event_type(), EventType::StateSnapshot);
}

/// A delta the client's default limits would refuse (40k replacements at
/// pointer depth 101) is smaller than the 12 MB snapshot, but must still go
/// out as a snapshot, so the client does not go stale. (Text is weighted in
/// the client's allowances: a much bigger blob would let the delta fit.)
#[test]
fn a_delta_the_client_would_refuse_is_sent_as_a_snapshot() {
    fn build(offset: u64) -> Value {
        let leaf: serde_json::Map<String, Value> = (0..40_000u64)
            .map(|i| (format!("k{i}"), json!(i + offset)))
            .collect();
        let mut v = Value::Object(leaf);
        for _ in 0..100 {
            v = json!({ "a": v });
        }
        json!({ "d": v, "blob": "x".repeat(12_000_000) })
    }
    let (before, after) = (build(0), build(1));
    let mut publisher = StatePublisher::new(before.clone());
    let event = publisher.publish(&after).unwrap();
    assert_eq!(event.event_type(), EventType::StateSnapshot);
    assert!(apply_all(before, &[event]) == after);
}

/// Drops the `secret` member from state on its way to the client.
struct Redact;

impl Transformer for Redact {
    fn transform(&mut self, event: Event, out: &mut Vec<Event>) {
        match event {
            Event::StateSnapshot(mut s) => {
                if let Some(o) = s.snapshot.as_object_mut() {
                    o.remove("secret");
                }
                out.push(s.into());
            }
            Event::StateDelta(mut d) => {
                d.delta
                    .retain(|op| !op.path.as_str().starts_with("/secret"));
                if !d.delta.is_empty() {
                    out.push(d.into());
                }
            }
            other => out.push(other),
        }
    }
}

#[test]
fn deltas_are_against_what_the_client_received_after_transformers() {
    // The unchanging pad makes a delta smaller than a snapshot, so the later
    // publishes go out as STATE_DELTA rather than a fresh snapshot.
    let pad = "p".repeat(4096);
    let agent_pad = pad.clone();
    let runner = Runner::new(agent(move |mut ctx| {
        let pad = agent_pad.clone();
        async move {
            *ctx.state_mut() = json!({"secret": "s1", "n": 1, "pad": pad});
            ctx.resync_state().await?;
            *ctx.state_mut() = json!({"secret": "s2", "n": 2, "pad": pad});
            ctx.publish_state().await?;
            *ctx.state_mut() = json!({"secret": "s3", "n": 3, "m": [1], "pad": pad});
            ctx.publish_state().await?;
            Ok(RunOutcome::success())
        }
    }))
    .transformer(|| Redact);
    let events = run(&runner, input());
    let deltas: Vec<&StateDelta> = events
        .iter()
        .filter_map(|e| match e {
            Event::StateDelta(d) => Some(d),
            _ => None,
        })
        .collect();
    assert!(!deltas.is_empty(), "no STATE_DELTA was sent: {events:?}");
    assert!(deltas
        .iter()
        .flat_map(|d| &d.delta)
        .all(|op| !op.path.as_str().starts_with("/secret")));
    let client = apply_all(json!({}), &events);
    assert_eq!(client, json!({"n": 3, "m": [1], "pad": pad}));
}

/// covers: STATE-09, STATE-14
#[test]
fn messages_snapshots_are_complete_and_unattributed() {
    let history = vec![
        Message::user("one"),
        Message::assistant("two"),
        Message::user("three"),
    ];
    let snapshot_history = history.clone();
    let runner = Runner::new(agent(move |ctx| {
        let history = snapshot_history.clone();
        async move {
            let sub = ctx.emitter().start_subagent("s").await?;
            sub.emitter().emit(MessagesSnapshot::new(history)).await?;
            sub.finish(None, None).await?;
            Ok(RunOutcome::success())
        }
    }));
    let events = run(&runner, input());
    let snapshot = events
        .iter()
        .find_map(|e| match e {
            Event::MessagesSnapshot(m) => Some(m.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(snapshot.messages, history);
    // Unattributed by construction: MessagesSnapshot has no subagentRunId
    // field, so its wire form cannot carry one.

    // A replayed run restates its complete history in order.
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::success()) }));
    let replay = vec![ReplayedRun::new("old".into(), history.clone(), json!({}))];
    let events = drain(
        runner
            .run_with_replay(
                replay,
                admitted(input()),
                CancelToken::new(),
                Extensions::new(),
            )
            .unwrap(),
    );
    let Event::MessagesSnapshot(m) = &events[1] else {
        panic!()
    };
    assert_eq!(m.messages, history);
}

/// covers: STATE-10
#[test]
fn state_carried_back_by_the_next_input_is_the_baseline() {
    let runner = Runner::new(agent(|mut ctx| async move {
        // The agent republishes what it was given: nothing to send.
        let current = ctx.state().clone();
        *ctx.state_mut() = current;
        ctx.publish_state().await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input().with_state(json!({"left": "behind"})));
    assert!(!types(&events).contains(&EventType::StateDelta));
    assert!(!types(&events).contains(&EventType::StateSnapshot));

    // A snapshot the agent emits directly moves the baseline too.
    let runner = Runner::new(agent(|mut ctx| async move {
        ctx.emitter()
            .emit(StateSnapshot::new(json!({"v": 1})))
            .await?;
        *ctx.state_mut() = json!({"v": 1});
        ctx.publish_state().await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input().with_state(json!({"v": 0})));
    assert_eq!(
        types(&events)
            .into_iter()
            .filter(|t| matches!(t, EventType::StateDelta | EventType::StateSnapshot))
            .count(),
        1
    );
}
