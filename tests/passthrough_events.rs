//! RAW and CUSTOM through the reducer (`events/passthrough.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;
mod support_reducer;
#[cfg(feature = "server")]
mod support_server;

use agui_sdk::Event;
use serde_json::json;
use support_reducer::{event, Harness};

/// covers(client): PASS-01, PASS-03
#[test]
fn raw_and_custom_are_standalone_and_change_nothing() {
    let mut h = Harness::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "a"}));
    // A RAW event that looks like protocol material drives nothing.
    let changes = h.push(
        json!({"type": "RAW", "event": {"type": "TEXT_MESSAGE_END", "messageId": "m1",
        "delta": "injected"}, "source": "provider"}),
    );
    assert_eq!(changes, Default::default());
    h.push(json!({"type": "CUSTOM", "name": "acme.progress", "value": {"pct": 50}}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "b"}));
    assert_eq!(
        h.history(),
        json!([{"id": "m1", "role": "assistant", "content": "ab"}])
    );
    assert_eq!(h.state(), &json!({}));
    assert!(h.codes().is_empty());

    // Through the whole pipeline (enforcement, chunk expansion, verification,
    // reducer). A RAW that looks like an end closes nothing: the message is
    // still open for the content after it.
    let r = support_pipeline::run(&support_pipeline::in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "a"}),
        json!({"type": "RAW", "event": {"type": "TEXT_MESSAGE_END", "messageId": "m1"}}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "b"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
    ]));
    r.assert_ok();
    assert_eq!(r.message("m1")["content"], "ab");
    // RAW stands aside from an open chunk stream: the id-less continuation
    // after it still continues the message, with nothing to warn about.
    let r = support_pipeline::run(&support_pipeline::in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "assistant", "delta": "Half "}),
        json!({"type": "RAW", "event": {"kind": "heartbeat"}}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "and half."}),
    ]));
    r.assert_ok();
    assert_eq!(r.messages.len(), 1);
    assert_eq!(r.message("m1")["content"], "Half and half.");
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    // CUSTOM, unlike RAW, ends the open chunk stream in its lane, so the
    // id-less continuation has nothing to continue.
    let r = support_pipeline::run(&support_pipeline::in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "assistant", "delta": "Half "}),
        json!({"type": "CUSTOM", "name": "acme.note", "value": {"closes": true}}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "and half."}),
    ]));
    r.assert_rejected("First TEXT_MESSAGE_CHUNK must have a messageId");
}

/// covers(client): PASS-02, PASS-05
#[test]
fn required_and_optional_members() {
    let raw = event(json!({"type": "RAW", "event": null}));
    let Event::Raw(r) = &raw else { panic!() };
    assert_eq!((r.event.clone(), r.source.clone()), (json!(null), None));
    let custom = event(json!({"type": "CUSTOM", "name": "acme.x", "value": null}));
    let Event::Custom(c) = &custom else { panic!() };
    assert_eq!((c.name.as_str(), c.value.clone()), ("acme.x", json!(null)));
    for bad in [
        json!({"type": "RAW"}),
        json!({"type": "CUSTOM", "value": 1}),
        json!({"type": "CUSTOM", "name": "x"}),
    ] {
        assert!(
            serde_json::from_value::<Event>(bad.clone()).is_err(),
            "{bad}"
        );
    }
}

/// covers: PASS-06
#[test]
fn unrecognised_custom_name_is_ignored_silently() {
    let mut h = Harness::new();
    let e = json!({"type": "CUSTOM", "name": "unknown-to-anyone", "value": {"x": [1]}});
    h.push(e.clone());
    assert!(h.codes().is_empty());
    assert_eq!(
        serde_json::to_value(event(e.clone())).unwrap(),
        e,
        "nothing stripped"
    );
}

/// covers: PASS-07
#[test]
fn invented_names_take_a_vendor_prefix() {
    assert_eq!(
        agui_sdk::metadata::vendor_key("acme", "progress"),
        "acme.progress"
    );
}

/// covers: PASS-09
#[test]
fn payloads_are_inert_data() {
    let mut h = Harness::new();
    let markup = "<img src=x onerror=alert(1)>";
    h.push(json!({"type": "CUSTOM", "name": "acme.render", "value": markup}));
    h.push(json!({"type": "RAW", "event": {"exec": "rm -rf /", "grant": "admin"}}));
    // Nothing was rendered into history, executed into state, or granted.
    assert!(h.reducer.messages().is_empty());
    assert_eq!(h.state(), &json!({}));
    assert!(h.reducer.pending_tool_calls().is_empty());
}

/// The producer half: the emitter sends RAW and CUSTOM exactly as given,
/// standalone inside the run, with an optional `source` left out when unset.
#[cfg(feature = "server")]
mod producer {
    use agui_sdk::server::{RunOutcome, Runner};
    use agui_sdk::Event;
    use serde_json::json;

    use super::support_server::{agent, input, run};

    /// covers(server): PASS-01, PASS-02, PASS-05
    #[test]
    fn raw_and_custom_are_sent_as_given() {
        let runner = Runner::new(agent(|ctx| async move {
            let e = ctx.emitter();
            e.raw(json!({"provider": [1, null]}), Some("acme".to_owned()))
                .await?;
            e.custom("acme.progress", json!({"pct": 50})).await?;
            e.raw(json!(null), None).await?;
            e.custom("acme.null", json!(null)).await?;
            Ok(RunOutcome::success())
        }));
        let events = run(&runner, input());
        let wire: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| matches!(e, Event::Raw(_) | Event::Custom(_)))
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        assert_eq!(
            wire,
            [
                json!({"type": "RAW", "event": {"provider": [1, null]}, "source": "acme"}),
                json!({"type": "CUSTOM", "name": "acme.progress", "value": {"pct": 50}}),
                json!({"type": "RAW", "event": null}),
                json!({"type": "CUSTOM", "name": "acme.null", "value": null}),
            ]
        );
        // Standalone: inside the run, between its start and its finish.
        assert!(matches!(events.first(), Some(Event::RunStarted(_))));
        assert!(matches!(events.last(), Some(Event::RunFinished(_))));
    }
}
