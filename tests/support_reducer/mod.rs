//! Shared harness for the reducer rule tests: events are written as wire
//! JSON, applied to a [`Reducer`], and history is compared as wire JSON.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use agui_sdk::diagnostics::{CollectingSink, Diagnostic, Reporter};
use agui_sdk::message::Message;
use agui_sdk::pipeline::reduce::{Changes, Reducer};
use agui_sdk::pipeline::Processed;
use agui_sdk::Event;
use serde_json::{json, Value};

/// Parses one wire event.
pub fn event(v: Value) -> Event {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("bad test event {v}: {e}"))
}

/// Parses wire messages.
pub fn messages(v: Value) -> Vec<Message> {
    serde_json::from_value(v).unwrap()
}

pub struct Harness {
    pub reducer: Reducer,
    pub sink: Arc<CollectingSink>,
    reporter: Reporter,
}

impl Harness {
    pub fn new() -> Self {
        Self::with(Reducer::new())
    }

    pub fn with_history(history: Value, state: Value) -> Self {
        Self::with(Reducer::with_history(messages(history), state))
    }

    fn with(reducer: Reducer) -> Self {
        let sink = Arc::new(CollectingSink::new());
        let reporter = Reporter::new(sink.clone(), 256);
        Harness {
            reducer,
            sink,
            reporter,
        }
    }

    /// Applies one wire event.
    pub fn push(&mut self, v: Value) -> Changes {
        self.push_processed(v).changes
    }

    /// Applies one wire event, returning the delivered event as well.
    pub fn push_processed(&mut self, v: Value) -> Processed {
        let e = event(v);
        let processed = self.reducer.apply(e, &mut self.reporter);
        self.reporter.end_event(None);
        processed
    }

    /// Applies several wire events.
    pub fn push_all(&mut self, events: Value) {
        let Value::Array(list) = events else {
            panic!("expected an array of events")
        };
        for e in list {
            self.push(e);
        }
    }

    /// History as wire JSON.
    pub fn history(&self) -> Value {
        serde_json::to_value(self.reducer.messages()).unwrap()
    }

    pub fn state(&self) -> &Value {
        self.reducer.state()
    }

    /// Diagnostics reported so far (drained).
    pub fn take(&self) -> Vec<Diagnostic> {
        self.sink.take()
    }

    /// Codes of the diagnostics reported so far (drained).
    pub fn codes(&self) -> Vec<&'static str> {
        self.take().iter().map(|d| d.code.as_str()).collect()
    }
}

pub fn run_started() -> Value {
    json!({"type": "RUN_STARTED", "threadId": "t1", "runId": "r1"})
}

pub fn run_finished() -> Value {
    json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": "r1"})
}
