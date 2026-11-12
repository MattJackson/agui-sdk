//! Server-side run input: admission, what the agent sees, and how the run's
//! identifiers echo the input.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_server;

use std::sync::{Arc, Mutex};

use agui_sdk::content::{Content, ContentPart, FileSource, MediaPart, PartSource, TextPart};
use agui_sdk::diagnostics::{CollectingSink, DiagnosticCode, Reporter, Scope};
use agui_sdk::event::Event;
use agui_sdk::ids::RunId;
use agui_sdk::message::{Message, UserMessage};
use agui_sdk::run_input::Context;
use agui_sdk::server::helpers::{context_prompt, PartFilter};
use agui_sdk::server::input::{admit, InputPolicy};
use agui_sdk::server::{
    Agent, CancelToken, Extensions, ReplayedRun, RunContext, RunOutcome, Runner, RunnerConfig,
};
use agui_sdk::{ErrorCode, RunAgentInput};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use support_server::*;

fn reporter() -> (Arc<CollectingSink>, Reporter) {
    let sink = Arc::new(CollectingSink::new());
    (sink.clone(), Reporter::new(sink, 256))
}

fn body(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

fn minimal_body() -> Value {
    json!({"threadId": "t1", "runId": "r1", "messages": [{"id": "m1", "role": "user", "content": "hi"}]})
}

/// covers: RUNIN-02
#[test]
fn input_echo_is_opt_in() {
    let mut config = RunnerConfig::default();
    config.echo_input = true;
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::success()) })).config(config);
    let input = input();
    let events = run(&runner, input.clone());
    let Event::RunStarted(started) = &events[0] else {
        panic!()
    };
    assert_eq!(started.input.as_deref(), Some(&input));
}

/// covers: RUNIN-04, RUNIN-05, RUNIN-07
#[test]
fn boundary_events_carry_the_inputs_identifiers() {
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::success()) }));
    let events = run(&runner, input().with_parent_run_id("parent"));
    let Event::RunStarted(started) = &events[0] else {
        panic!()
    };
    let finished = finished(&events);
    assert_eq!(started.thread_id, finished.thread_id);
    assert_eq!(started.run_id, finished.run_id);
    assert_eq!(started.thread_id.as_str(), "t1");
    assert_eq!(started.run_id.as_str(), "r1");
    assert_eq!(started.parent_run_id.as_ref().unwrap().as_str(), "parent");
}

/// covers: RUNIN-06, RUNIN-31
#[test]
fn replayed_runs_keep_their_own_ids_and_the_requested_run_is_last() {
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::success()) }));
    let replay = vec![ReplayedRun::new(RunId::new("old"), vec![], json!({}))];
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
    let ids: Vec<(String, String)> = events
        .iter()
        .filter_map(|e| match e {
            Event::RunStarted(s) => Some((s.thread_id.to_string(), s.run_id.to_string())),
            Event::RunFinished(f) => Some((f.thread_id.to_string(), f.run_id.to_string())),
            _ => None,
        })
        .collect();
    let expected = [("t1", "old"), ("t1", "old"), ("t1", "r1"), ("t1", "r1")];
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(ids, expected);
}

/// covers: RUNIN-10, RUNIN-20, RUNIN-23, RUNIN-27
#[test]
fn the_agent_sees_the_complete_history_and_empty_collections() {
    let seen = Arc::new(Mutex::new(None));
    let seen2 = Arc::clone(&seen);
    let runner = Runner::new(agent(move |ctx| {
        let seen = Arc::clone(&seen2);
        async move {
            *seen.lock().unwrap() = Some((
                ctx.messages().len(),
                ctx.tools().len(),
                ctx.context().len(),
                ctx.forwarded_props().cloned(),
            ));
            Ok(RunOutcome::success())
        }
    }));
    let mut value = minimal_body();
    value["messages"] = json!([
        {"id": "m1", "role": "user", "content": "one"},
        {"id": "m2", "role": "assistant", "content": "two"},
        {"id": "m3", "role": "user", "content": "three"}
    ]);
    value["forwardedProps"] = json!({"x-vendor": {"nested": [1, null, "s"]}});
    let admitted = admit(&body(value), &InputPolicy::default()).unwrap();
    drain(
        runner
            .run(admitted, CancelToken::new(), Extensions::new())
            .unwrap(),
    );
    let (messages, tools, context, forwarded) = seen.lock().unwrap().clone().unwrap();
    assert_eq!((messages, tools, context), (3, 0, 0));
    assert_eq!(
        forwarded,
        Some(json!({"x-vendor": {"nested": [1, null, "s"]}}))
    );
}

fn image_file(value: &str, provider: &str) -> ContentPart {
    ContentPart::Image(MediaPart::new(PartSource::File(
        FileSource::new(value).with_provider(provider),
    )))
}

/// covers: RUNIN-11, RUNIN-15, RUNIN-16
#[test]
fn unusable_parts_are_skipped_with_a_warning_and_the_run_continues() {
    let filter = PartFilter::new(|part| match part {
        ContentPart::Image(media) => {
            !matches!(&media.source, PartSource::File(f) if f.value == "expired-handle")
        }
        _ => true,
    });
    let (sink, mut warnings) = reporter();
    let mut message = Message::User(UserMessage::new(
        "m1",
        Content::Parts(vec![
            ContentPart::Text(TextPart::new("look")),
            image_file("expired-handle", "anthropic"),
            image_file("good-handle", "anthropic"),
        ]),
    ));
    filter.filter_message(&mut message, &mut warnings);
    let Message::User(user) = &message else {
        panic!()
    };
    let Content::Parts(parts) = &user.content else {
        panic!()
    };
    assert_eq!(parts.len(), 2);
    let diagnostics = sink.take();
    assert_eq!(codes(&diagnostics), [DiagnosticCode::ContentPartSkipped]);
    assert_eq!(diagnostics[0].pointer, "/content/1");

    // Inside a run the same filter lets the agent carry on.
    let runner = Runner::new(agent(|ctx| async move {
        let filter = PartFilter::text_only();
        let (_sink, mut warnings) = reporter();
        let mut messages = ctx.messages().to_vec();
        filter.filter_messages(&mut messages, &mut warnings);
        ctx.emitter().say("answered from text").await?;
        Ok(RunOutcome::success())
    }));
    let input = RunAgentInput::new(
        "t1",
        "r1",
        vec![Message::User(UserMessage::new(
            "m1",
            Content::Parts(vec![image_file("h", "openai")]),
        ))],
    );
    let events = run(&runner, input);
    assert_eq!(
        finished(&events).outcome,
        Some(agui_sdk::lifecycle::RunFinishedOutcome::success())
    );
}

/// Parts skipped in different messages of a run input get pointers rooted in
/// that input, so they stay apart.
///
/// covers: RUNIN-11
#[test]
fn skipped_parts_across_messages_get_run_input_pointers() {
    let mut messages: Vec<Message> = ["m1", "m2"]
        .into_iter()
        .map(|id| {
            Message::User(UserMessage::new(
                id,
                Content::Parts(vec![
                    ContentPart::Text(TextPart::new("look")),
                    image_file("h", "openai"),
                ]),
            ))
        })
        .collect();
    let (sink, mut warnings) = reporter();
    PartFilter::text_only().filter_messages(&mut messages, &mut warnings);
    let diagnostics = sink.take();
    assert_eq!(
        codes(&diagnostics),
        [
            DiagnosticCode::ContentPartSkipped,
            DiagnosticCode::ContentPartSkipped
        ]
    );
    let pointers: Vec<&str> = diagnostics.iter().map(|d| d.pointer.as_str()).collect();
    assert_eq!(pointers, ["/messages/0/content/1", "/messages/1/content/1"]);
    assert!(diagnostics.iter().all(|d| d.scope == Scope::RunInput));
    for message in &messages {
        let Message::User(user) = message else {
            panic!()
        };
        assert_eq!(
            user.content,
            Content::Parts(vec![ContentPart::Text(TextPart::new("look"))])
        );
    }
}

/// covers: RUNIN-13, RUNIN-14
#[test]
fn file_handles_are_carried_opaquely() {
    let mut value = minimal_body();
    value["messages"] = json!([{"id": "m1", "role": "user", "content": [
        {"type": "image", "source": {"type": "file", "value": "file-abc://not-a-url?x=1", "provider": "anthropic"}}
    ]}]);
    let admitted = admit(&body(value), &InputPolicy::default()).unwrap();
    let Message::User(user) = &admitted.input.messages[0] else {
        panic!()
    };
    let Content::Parts(parts) = &user.content else {
        panic!()
    };
    let ContentPart::Image(media) = &parts[0] else {
        panic!()
    };
    let PartSource::File(file) = &media.source else {
        panic!()
    };
    // Never parsed, fetched or rewritten: the value is exactly what was sent,
    // and the provider is the same lowercase id a TokenUsage would carry.
    assert_eq!(file.value, "file-abc://not-a-url?x=1");
    assert_eq!(file.provider.as_deref(), Some("anthropic"));
    let filtered =
        PartFilter::new(|_| true).filter_result(Content::Parts(parts.clone()), &mut reporter().1);
    assert_eq!(filtered, Content::Parts(parts.clone()));
}

/// covers: RUNIN-18
#[test]
fn malformed_input_is_rejected_before_any_stream() {
    let mut value = minimal_body();
    value["messages"] = json!([{"id": "m1", "role": "user", "content": [
        {"type": "image", "source": {"type": "ftp", "value": "x"}}
    ]}]);
    // A source of unknown kind fails validation: the part never reaches the
    // agent (enforcement removes it whole, with a warning).
    let admitted = admit(&body(value), &InputPolicy::default()).unwrap();
    let Message::User(user) = &admitted.input.messages[0] else {
        panic!()
    };
    assert!(!matches!(&user.content, Content::Parts(parts) if !parts.is_empty()));
    assert!(!admitted.diagnostics.is_empty());

    let error = admit(b"{not json", &InputPolicy::default()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::InputRejected);
    assert_eq!(error.http_status(), Some(400));

    let mut policy = InputPolicy::default();
    policy.max_body_bytes = 8;
    let error = admit(&body(minimal_body()), &policy).unwrap_err();
    assert_eq!(error.code(), ErrorCode::RequestTooLarge);
    assert_eq!(error.http_status(), Some(413));
}

/// covers: RUNIN-22
#[test]
fn every_context_entry_reaches_the_prompt() {
    let context = [
        Context::new("user locale", "en-GB"),
        Context::new("open document", "chapter 3"),
    ];
    let prompt = context_prompt(&context);
    assert_eq!(prompt, "user locale: en-GB\n\nopen document: chapter 3");
    assert_eq!(context_prompt(&[]), "");

    // What an agent actually gets: the context entries the client sent,
    // all of them and in order, through admission and `RunContext`.
    let seen = Arc::new(Mutex::new(None));
    let seen2 = Arc::clone(&seen);
    let runner = Runner::new(agent(move |ctx| {
        let seen = Arc::clone(&seen2);
        async move {
            *seen.lock().unwrap() = Some(context_prompt(ctx.context()));
            Ok(RunOutcome::success())
        }
    }));
    let mut value = minimal_body();
    value["context"] = json!([
        {"description": "user locale", "value": "en-GB"},
        {"description": "open document", "value": "chapter 3"},
        {"description": "time zone", "value": "Europe/London"}
    ]);
    let admitted = admit(&body(value), &InputPolicy::default()).unwrap();
    drain(
        runner
            .run(admitted, CancelToken::new(), Extensions::new())
            .unwrap(),
    );
    assert_eq!(
        seen.lock().unwrap().as_deref(),
        Some("user locale: en-GB\n\nopen document: chapter 3\n\ntime zone: Europe/London")
    );
}

#[derive(Debug, Serialize, Deserialize)]
struct Counter {
    #[serde(default)]
    count: u32,
}

struct Counting(Arc<Mutex<Option<u32>>>);

impl Agent for Counting {
    type State = Counter;

    async fn run(
        &self,
        mut ctx: RunContext<Counter>,
    ) -> Result<RunOutcome, agui_sdk::server::BoxError> {
        *self.0.lock().unwrap() = Some(ctx.state().count);
        ctx.state_mut().count += 1;
        ctx.publish_state().await?;
        Ok(RunOutcome::success())
    }
}

/// covers: RUNIN-24, RUNIN-25
#[test]
fn state_starts_from_the_input_and_deltas_are_against_it() {
    let seen = Arc::new(Mutex::new(None));
    let runner = Runner::new(Counting(Arc::clone(&seen)));

    // Absent state is `{}`: the typed state is its default, and what is
    // published brings a client holding `{}` to the new state.
    let events = run(&runner, input());
    assert_eq!(*seen.lock().unwrap(), Some(0));
    assert_eq!(client_after(json!({}), &events), json!({"count": 1}));

    // Present state: the run starts from it, and the delta is against it
    // (no snapshot first, RUNIN-25).
    let long = "kept ".repeat(40);
    let untyped = Runner::new(agent(|mut ctx| async move {
        let next = ctx.state()["count"].as_u64().unwrap_or(0) + 1;
        ctx.state_mut()["count"] = json!(next);
        ctx.publish_state().await?;
        Ok(RunOutcome::success())
    }));
    let events = run(
        &untyped,
        input().with_state(json!({"count": 41, "other": long})),
    );
    assert!(!types(&events).contains(&agui_sdk::EventType::StateSnapshot));
    assert!(types(&events).contains(&agui_sdk::EventType::StateDelta));
    let client = client_after(json!({"count": 41, "other": long}), &events);
    assert_eq!(client["count"], json!(42));
}

fn client_after(mut client: Value, events: &[Event]) -> Value {
    for event in events {
        match event {
            Event::StateSnapshot(s) => client = s.snapshot.clone(),
            Event::StateDelta(d) => {
                let mut budget =
                    agui_sdk::patch::PatchBudget::for_document(&client, Default::default());
                agui_sdk::patch::apply(&mut client, &d.delta, &mut budget).unwrap();
            }
            _ => {}
        }
    }
    client
}

#[test]
fn request_data_reaches_the_agent_through_extensions() {
    #[derive(Debug, PartialEq)]
    struct Principal(&'static str);
    let seen = Arc::new(Mutex::new(None));
    let seen2 = Arc::clone(&seen);
    let runner = Runner::new(agent(move |ctx| {
        let seen = Arc::clone(&seen2);
        async move {
            *seen.lock().unwrap() = ctx.extensions().get::<Principal>().map(|p| p.0);
            Ok(RunOutcome::success())
        }
    }));
    let mut extensions = Extensions::new();
    extensions.insert(Principal("alice"));
    drain(
        runner
            .run(admitted(input()), CancelToken::new(), extensions)
            .unwrap(),
    );
    assert_eq!(*seen.lock().unwrap(), Some("alice"));
}

/// covers: RUNIN-29
#[test]
fn a_state_that_does_not_fit_is_refused_before_the_stream() {
    let runner = Runner::new(Counting(Arc::new(Mutex::new(None))));
    let error = runner
        .run(
            admitted(input().with_state(json!({"count": "not a number"}))),
            CancelToken::new(),
            Extensions::new(),
        )
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InputRejected);
    assert_eq!(error.http_status(), Some(400));
}

/// covers: RUNIN-30
#[test]
fn unknown_members_are_stripped_with_a_warning() {
    let mut value = minimal_body();
    value["x-unknown"] = json!({"anything": true});
    let admitted = admit(&body(value), &InputPolicy::default()).unwrap();
    assert!(admitted
        .diagnostics
        .iter()
        .any(|d| d.code == DiagnosticCode::UnknownField && d.pointer.contains("x-unknown")));
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::success()) }));
    let events = drain(
        runner
            .run(admitted, CancelToken::new(), Extensions::new())
            .unwrap(),
    );
    assert_eq!(
        finished(&events).outcome,
        Some(agui_sdk::lifecycle::RunFinishedOutcome::success())
    );
}

/// Admission's own capped list reaches the sink as it is: its summary and
/// the version note after it are not capped a second time.
#[test]
fn admission_diagnostics_reach_the_sink_uncapped_again() {
    let mut value = minimal_body();
    for i in 0..300 {
        value[format!("x{i}")] = json!(i);
    }
    value["protocolVersion"] = json!("1.9");
    let admitted = admit(&body(value), &InputPolicy::default()).unwrap();
    let listed = admitted.diagnostics.clone();
    let sink = Arc::new(CollectingSink::new());
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::success()) }))
        .diagnostic_sink(sink.clone());
    let stream = runner
        .run(admitted, CancelToken::new(), Extensions::new())
        .unwrap();
    drain(stream);
    let reported = sink.take();
    let codes = codes(&reported);
    assert!(
        codes.contains(&DiagnosticCode::NewerMinorVersion),
        "{codes:?}"
    );
    let limits: Vec<_> = reported
        .iter()
        .filter(|d| d.code == DiagnosticCode::Limit)
        .collect();
    assert_eq!(limits.len(), 1, "{limits:?}");
    // Admission's own summary, not a recount of what the runner replayed.
    assert_eq!(reported, listed);
}
