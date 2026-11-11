//! Producer-side activity messages (`events/activity.mdx`): snapshot
//! baselines, deltas, ownership and the object-shaped content.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_producer;

use agui_sdk::pipeline::verify::ProducerRule;
use agui_sdk::server::{RunOutcome, Runner};
use serde_json::{json, Value};
use support_producer::{accepts, agent, finished, refuses, started};

fn snapshot(owner: Option<&str>, replace: Option<bool>) -> Value {
    let mut e = json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a1", "activityType": "search", "content": {"q": "x"}});
    if let Some(owner) = owner {
        e["subagentRunId"] = json!(owner);
    }
    if let Some(replace) = replace {
        e["replace"] = json!(replace);
    }
    e
}

fn delta(patch: Value) -> Value {
    json!({"type": "ACTIVITY_DELTA", "messageId": "a1", "activityType": "search", "patch": patch})
}

/// covers: ACT-08
#[test]
fn a_keeping_snapshot_agrees_with_the_owner() {
    refuses(
        &[
            started("r1"),
            snapshot(Some("s1"), None),
            snapshot(Some("s2"), Some(false)),
        ],
        2,
        ProducerRule::OwnerMismatch,
    );
    accepts(&[
        started("r1"),
        snapshot(Some("s1"), None),
        snapshot(Some("s1"), Some(false)),
        snapshot(None, Some(false)),
        finished("r1"),
    ]);
}

/// covers: ACT-10
#[test]
fn a_delta_repeats_the_activity_type() {
    // The field is required by the type, so it is always sent.
    let missing: Result<agui_sdk::Event, _> =
        serde_json::from_value(json!({"type": "ACTIVITY_DELTA", "messageId": "a1", "patch": []}));
    assert!(missing.is_err());
    accepts(&[
        started("r1"),
        snapshot(None, None),
        delta(json!([{"op": "add", "path": "/hits", "value": 3}])),
        finished("r1"),
    ]);
}

/// covers: ACT-11
#[test]
fn a_delta_needs_a_snapshot_first() {
    let message = refuses(
        &[
            started("r1"),
            delta(json!([{"op": "add", "path": "/n", "value": 1}])),
        ],
        1,
        ProducerRule::NoBaseline,
    );
    assert!(message.contains("ACT-11"), "{message}");
}

/// covers: ACT-15
#[test]
fn content_stays_an_object() {
    for patch in [
        json!([{"op": "replace", "path": "", "value": [1]}]),
        json!([{"op": "add", "path": "", "value": "text"}]),
        json!([{"op": "remove", "path": ""}]),
        json!([{"op": "move", "from": "/q", "path": ""}]),
    ] {
        refuses(
            &[started("r1"), snapshot(None, None), delta(patch)],
            2,
            ProducerRule::InvalidValue,
        );
    }
    accepts(&[
        started("r1"),
        snapshot(None, None),
        delta(
            json!([{"op": "replace", "path": "", "value": {"q": "y"}}, {"op": "remove", "path": "/q"}]),
        ),
        finished("r1"),
    ]);
}

/// covers: ACT-17
#[test]
fn an_activity_takes_its_own_message_id_in_history() {
    // Its id is a message id in the thread: another message cannot reuse it.
    refuses(
        &[
            started("r1"),
            snapshot(None, None),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "a1"}),
        ],
        2,
        ProducerRule::DuplicateMessageId,
    );
    // Through the runner, the emitter's snapshot and delta pass.
    let runner = Runner::new(agent(|ctx| async move {
        let id = agui_sdk::ids::MessageId::new("act-1");
        ctx.emitter()
            .activity_snapshot(id.clone(), "search", agui_sdk::json::JsonObject::new())
            .await?;
        ctx.emitter()
            .activity_delta(
                id,
                "search",
                vec![agui_sdk::patch::PatchOperation::add(
                    agui_sdk::patch::JsonPointer::parse("/n").unwrap(),
                    json!(1),
                )],
            )
            .await?;
        Ok(RunOutcome::success())
    }));
    let rt = support_producer::runtime();
    let events = rt.block_on(async {
        use agui_sdk::server::input::{admit, InputPolicy};
        let admitted = admit(
            &support_producer::input_json("t1", "r1"),
            &InputPolicy::default(),
        )
        .unwrap();
        let mut stream = runner
            .run(
                admitted,
                agui_sdk::server::CancelToken::new(),
                agui_sdk::server::Extensions::new(),
            )
            .unwrap();
        let mut out = Vec::new();
        while let Some(event) = stream.next().await {
            out.push(event.event_type());
        }
        out
    });
    assert!(
        events.contains(&agui_sdk::EventType::ActivityDelta),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&agui_sdk::EventType::RunFinished));
}
