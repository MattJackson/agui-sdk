//! Drivers for the pipeline rule tests.
//!
//! Streams are written as JSON event objects. [`run`] feeds them through
//! [`Processor::push_json`] (the fast path), [`run_value`] through
//! [`Processor::push_value`] (the middleware path). [`expand`] and [`verify`]
//! drive the chunk expander and the consumer verifier on their own, for
//! tests about one stage's output.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use agui_sdk::diagnostics::{CollectingSink, Diagnostic, DiagnosticCode, Reporter};
use agui_sdk::event::Event;
use agui_sdk::pipeline::chunks::ChunkExpander;
use agui_sdk::pipeline::verify::{ConsumerVerifier, RunPhase};
use agui_sdk::pipeline::{PipelineConfig, Processor, Termination};
use agui_sdk::Error;
use serde_json::{json, Value};

/// What a stream produced.
pub struct Run {
    /// Events delivered to the application, in order.
    pub events: Vec<Event>,
    /// The fatal verdict, if the stream was rejected.
    pub error: Option<Error>,
    /// How the stream ended (after the last frame fed).
    pub termination: Termination,
    /// History, serialized.
    pub messages: Vec<Value>,
    /// State.
    pub state: Value,
    /// Every diagnostic reported.
    pub diagnostics: Vec<Diagnostic>,
}

impl Run {
    /// The error message; panics if the stream was accepted.
    pub fn error_text(&self) -> String {
        match &self.error {
            Some(err) => format!("{}: {}", err.code().as_str(), err.message()),
            None => panic!("stream was accepted; events: {:?}", self.types()),
        }
    }

    /// Asserts the stream was rejected with a message containing `needle`.
    pub fn assert_rejected(&self, needle: &str) {
        let text = self.error_text();
        assert!(
            text.contains(needle),
            "error {text:?} does not contain {needle:?}"
        );
    }

    /// Asserts the stream was accepted and complete.
    pub fn assert_ok(&self) {
        if let Some(err) = &self.error {
            panic!(
                "stream rejected: {}: {}",
                err.code().as_str(),
                err.message()
            );
        }
        assert_eq!(self.termination, Termination::Complete);
    }

    /// Delivered event types.
    pub fn types(&self) -> Vec<&'static str> {
        types(&self.events)
    }

    /// Delivered events, serialized.
    pub fn wire(&self) -> Vec<Value> {
        self.events
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }

    /// The history message with `id`, serialized.
    pub fn message(&self, id: &str) -> &Value {
        self.messages
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("no message {id:?} in {:?}", self.messages))
    }

    /// Whether a diagnostic with `code` was reported.
    pub fn warned(&self, code: DiagnosticCode) -> bool {
        self.diagnostics.iter().any(|d| d.code == code)
    }
}

/// Event types of `events`.
pub fn types(events: &[Event]) -> Vec<&'static str> {
    events.iter().map(|e| e.event_type().as_str()).collect()
}

fn drive(stream: &[Value], value_path: bool) -> Run {
    drive_with(
        Processor::new(PipelineConfig::default()),
        stream,
        value_path,
    )
}

/// Feeds `stream` to `processor`, stopping at the first error.
pub fn drive_with(mut processor: Processor, stream: &[Value], value_path: bool) -> Run {
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 256);
    let mut out = Vec::new();
    let mut error = None;
    for frame in stream {
        let result = if value_path {
            processor.push_value(frame.clone(), &mut reporter, &mut out)
        } else {
            let bytes = serde_json::to_vec(frame).unwrap();
            processor.push_json(&bytes, &mut reporter, &mut out)
        };
        if let Err(err) = result {
            error = Some(err);
            break;
        }
    }
    let termination = if error.is_some() {
        processor.finish()
    } else {
        match processor.finish_checked() {
            Ok(termination) => termination,
            Err(err) => {
                error = Some(err);
                Termination::Complete
            }
        }
    };
    let reducer = processor.into_reducer();
    Run {
        events: out
            .into_iter()
            .map(|p| Arc::try_unwrap(p.event).unwrap_or_else(|shared| (*shared).clone()))
            .collect(),
        error,
        termination,
        messages: reducer
            .messages()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect(),
        state: reducer.state().clone(),
        diagnostics: sink.take(),
    }
}

/// Runs `stream` through the fast path.
pub fn run(stream: &[Value]) -> Run {
    drive(stream, false)
}

/// Runs `stream` through the middleware path.
pub fn run_value(stream: &[Value]) -> Run {
    drive(stream, true)
}

/// Parses one event as written (no enforcement).
pub fn event(value: Value) -> Event {
    serde_json::from_value(value).expect("test event parses")
}

/// Chunk expansion alone.
pub fn expand(stream: &[Value]) -> Result<Vec<Event>, Error> {
    let mut expander = ChunkExpander::new();
    let mut out = Vec::new();
    for frame in stream {
        expander.push(event(frame.clone()), &mut out)?;
    }
    Ok(out)
}

/// Chunk expansion and verification, without enforcement or the reducer.
/// Returns the phase at the end.
pub fn verify(stream: &[Value]) -> Result<RunPhase, Error> {
    let mut expander = ChunkExpander::new();
    let mut verifier = ConsumerVerifier::new();
    let mut out = Vec::new();
    for frame in stream {
        out.clear();
        expander.push(event(frame.clone()), &mut out)?;
        for e in &out {
            verifier.observe(e)?;
        }
    }
    Ok(verifier.phase())
}

/// `RUN_STARTED` for thread `t`, run `run_id`.
pub fn started(run_id: &str) -> Value {
    json!({"type": "RUN_STARTED", "threadId": "t", "runId": run_id})
}

/// `RUN_FINISHED` for thread `t`, run `run_id`.
pub fn finished(run_id: &str) -> Value {
    json!({"type": "RUN_FINISHED", "threadId": "t", "runId": run_id})
}

/// `RUN_ERROR`.
pub fn errored(message: &str) -> Value {
    json!({"type": "RUN_ERROR", "message": message})
}

/// A whole text triad.
pub fn text(id: &str, delta: &str) -> [Value; 3] {
    [
        json!({"type": "TEXT_MESSAGE_START", "messageId": id, "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": id, "delta": delta}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": id}),
    ]
}

/// `[RUN_STARTED r1, body.., RUN_FINISHED r1]`.
pub fn in_run(body: &[Value]) -> Vec<Value> {
    let mut stream = vec![started("r1")];
    stream.extend_from_slice(body);
    stream.push(finished("r1"));
    stream
}
