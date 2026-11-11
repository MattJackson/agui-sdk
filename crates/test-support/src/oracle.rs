//! The stream-fixture oracle: runs a fixture through the client pipeline and
//! judges what happened against the fixture's `expect` block.
//!
//! Every expectation key the corpus README defines is understood here
//! ([`EXPECT_KEYS`]). A key outside that list fails the fixture: supporting a
//! new key is a deliberate change to this file, never a silent pass. A lane
//! that genuinely cannot observe a key (the frames lane never sends a request)
//! names it in [`JudgeOptions::unobservable`], and the judge reports it as
//! skipped rather than met.

use std::sync::Arc;

use serde_json::{Map, Value};

use agui_sdk::diagnostics::{CollectingSink, Diagnostic, NullSink, Reporter};
use agui_sdk::pipeline::compat::{Boundary, EraShims};
use agui_sdk::pipeline::{PipelineConfig, Processed, Processor, Termination};
use agui_sdk::version::LegacyEra;

use crate::corpus::StreamFixture;
use crate::mock::{Delivery, MockTransport};
use crate::overrides::Overrides;

/// Every expectation key the corpus README defines, and so every key this
/// oracle can judge.
pub const EXPECT_KEYS: &[&str] = &[
    "outcome",
    "errorContains",
    "runError",
    "eventTypes",
    "eventTypesAbsent",
    "eventPaths",
    "eventAbsentPaths",
    "warnings",
    "noWarnings",
    "messageCount",
    "messages",
    "messageAbsentPaths",
    "state",
    "request",
    "requestAbsentPaths",
];

/// Whether the client consumed the stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Outcome {
    /// The client consumed the stream (a delivered `RUN_ERROR` included).
    #[default]
    Completed,
    /// The client rejected the stream; the text is the surfaced error
    /// (code and message).
    Failed(String),
    /// The harness could not run the fixture (unparsable input, a client that
    /// would not build); the stream was never judged, so no expectation about
    /// the client's behaviour is met.
    Harness(String),
}

/// What the pipeline did with a fixture's stream.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Observed {
    /// Events delivered to application code, as JSON, in order.
    pub events: Vec<Value>,
    /// Every warning, rendered as `code event-type pointer message`.
    pub warnings: Vec<String>,
    /// Final history as a JSON array.
    pub messages: Value,
    /// Final state.
    pub state: Value,
    /// The `RunAgentInput` the client sent, when the lane sends one.
    pub request: Option<Value>,
    /// Whether the client consumed or rejected the stream.
    pub outcome: Outcome,
}

impl Default for Observed {
    fn default() -> Self {
        Observed {
            events: Vec::new(),
            warnings: Vec::new(),
            messages: Value::Array(Vec::new()),
            state: Value::Object(Map::new()),
            request: None,
            outcome: Outcome::Completed,
        }
    }
}

impl Observed {
    /// An empty observation: no events, empty history, state `{}`, completed.
    pub fn new() -> Self {
        Self::default()
    }

    /// The delivered event types, in order.
    pub fn delivered_types(&self) -> Vec<&str> {
        self.events
            .iter()
            .map(|e| e.get("type").and_then(Value::as_str).unwrap_or(""))
            .collect()
    }
}

/// Renders a diagnostic as the text `warnings` substrings are matched against:
/// code, event type, pointer and message.
pub fn render_warning(d: &Diagnostic) -> String {
    format!(
        "{} {} {} {}",
        d.code,
        d.event_type
            .map(|t| t.as_str())
            .or(d.unknown_type.as_deref())
            .unwrap_or("-"),
        if d.pointer.is_empty() {
            "/"
        } else {
            &d.pointer
        },
        d.message
    )
}

fn render_error(e: &agui_sdk::Error) -> String {
    format!("{}: {}", e.code(), e)
}

/// Which `Processor` entry point a frames-level run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Way {
    /// `Processor::push_json` (the fast path).
    Json,
    /// `Processor::push_value` (the path middleware forces).
    Value,
}

fn pipeline_config(fixture: &StreamFixture) -> PipelineConfig {
    let mut config = PipelineConfig::default();
    config.legacy_peer = fixture
        .max_protocol_version()
        .and_then(agui_sdk::version::LegacyEra::parse);
    config
}

/// The fixture's input messages as typed history, after the always-on
/// outgoing upgrade every client applies (`Boundary::upgrade_input`: a legacy
/// `binary` part becomes typed media), so legacy input shapes can be held at
/// all. When `era` is set, the era's outgoing downgrade also runs on a copy,
/// as the client's request path would, so its warnings (lossy flattening)
/// reach `reporter`; the request itself is not observed here.
fn prepared_history(
    fixture: &StreamFixture,
    era: Option<LegacyEra>,
    reporter: &mut Reporter,
) -> Result<Vec<agui_sdk::message::Message>, String> {
    let Some(list) = fixture.input_messages() else {
        return Ok(Vec::new());
    };
    let mut input = Value::Object(Map::from_iter([(
        "messages".to_owned(),
        Value::Array(list.to_vec()),
    )]));
    Boundary::upgrade_input(&mut input, reporter);
    if let Some(era) = era {
        let mut request = input.clone();
        EraShims::new(era).downgrade_input(&mut request, reporter);
    }
    reporter.end_event(None);
    let messages = match input {
        Value::Object(mut map) => map.remove("messages").unwrap_or(Value::Null),
        _ => Value::Null,
    };
    match messages {
        Value::Array(items) => items
            .into_iter()
            .map(|m| {
                serde_json::from_value(m)
                    .map_err(|e| format!("harness: input message does not parse: {e}"))
            })
            .collect(),
        _ => Err("harness: input has no message list".to_owned()),
    }
}

/// [`prepared_history`] for the client lanes: the client runs the outgoing
/// steps (and reports them) itself, so only the upgrade that makes the input
/// typeable runs here, silently.
fn input_messages(fixture: &StreamFixture) -> Result<Vec<agui_sdk::message::Message>, String> {
    let mut quiet = Reporter::new(Arc::new(NullSink), 0);
    prepared_history(fixture, None, &mut quiet)
}

/// Runs a fixture's frames straight into a [`Processor`] and records what
/// happened. The input's messages seed the history. This lane sends no
/// request, so `request` and `requestAbsentPaths` are unobservable on it.
pub fn observe(fixture: &StreamFixture, way: Way) -> Observed {
    let mut observed = Observed::new();
    let config = pipeline_config(fixture);
    let era = config.legacy_peer;
    let per_event = config.limits.max_diagnostics_per_event;
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), per_event);
    let history = match prepared_history(fixture, era, &mut reporter) {
        Ok(h) => h,
        Err(e) => {
            observed.outcome = Outcome::Harness(e);
            return observed;
        }
    };
    let mut processor = Processor::with_history(config, history, Value::Object(Map::new()));
    // The middleware path owns the boundary and era shims (they run before
    // middleware); `push_json` runs its own.
    let mut boundary = Boundary::new();
    let shims = era.map(EraShims::new);
    let mut out: Vec<Processed> = Vec::new();
    let mut failed = None;
    let mut harness = None;
    for event in fixture.stream() {
        let result = match way {
            Way::Json => match serde_json::to_vec(event) {
                Ok(bytes) => processor.push_json(&bytes, &mut reporter, &mut out),
                Err(e) => {
                    harness = Some(format!("harness: {e}"));
                    break;
                }
            },
            Way::Value => {
                let mut frame = event.clone();
                let keep = boundary.upgrade(&mut frame, &mut reporter)
                    && shims
                        .as_ref()
                        .map_or(true, |s| s.upgrade_event(&mut frame, &mut reporter));
                if keep {
                    processor.push_value(frame, &mut reporter, &mut out)
                } else {
                    Ok(())
                }
            }
        };
        reporter.end_event(event.get("type").and_then(Value::as_str));
        if let Err(e) = result {
            failed = Some(render_error(&e));
            break;
        }
    }
    if failed.is_none() && harness.is_none() && processor.finish() == Termination::Truncated {
        failed = Some("stream.truncated: the stream ended inside a run".to_owned());
    }
    observed.outcome = match harness {
        Some(e) => Outcome::Harness(e),
        None => failed.map_or(Outcome::Completed, Outcome::Failed),
    };
    observed.events = out
        .iter()
        .map(|p| serde_json::to_value(&*p.event).unwrap_or(Value::Null))
        .collect();
    observed.messages = serde_json::to_value(processor.reducer().messages()).unwrap_or(Value::Null);
    observed.state = processor.reducer().state().clone();
    observed.warnings = sink.take().iter().map(render_warning).collect();
    observed
}

/// The peer the client is pinned to: `client.maxProtocolVersion` as a
/// pre-versioning era when it names one, else as a versioned peer, else
/// current.
pub fn peer_pin(fixture: &StreamFixture) -> agui_sdk::version::PeerPin {
    use agui_sdk::version::{PeerPin, ProtocolVersion};
    match fixture.max_protocol_version() {
        None => PeerPin::Current,
        Some(v) => LegacyEra::parse(v)
            .map(PeerPin::Legacy)
            .or_else(|| ProtocolVersion::parse(v).map(PeerPin::Version))
            .unwrap_or(PeerPin::Current),
    }
}

fn first_run_ids(fixture: &StreamFixture) -> Option<(String, String)> {
    let started = fixture
        .stream()
        .iter()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("RUN_STARTED"))?;
    Some((
        started.get("threadId")?.as_str()?.to_owned(),
        started.get("runId")?.as_str()?.to_owned(),
    ))
}

/// Runs a fixture through [`AgentClient`](agui_sdk::client::AgentClient) over
/// a [`MockTransport`] delivering it as `delivery`, and records what happened,
/// including the request the client sent.
pub async fn observe_client(fixture: &StreamFixture, delivery: Delivery) -> Observed {
    use agui_sdk::client::{AgentClient, RunRequest};

    let mut observed = Observed::new();
    let history = match input_messages(fixture) {
        Ok(h) => h,
        Err(e) => {
            observed.outcome = Outcome::Harness(e);
            return observed;
        }
    };
    let transport = MockTransport::new(fixture.stream().to_vec(), delivery);
    let sink = Arc::new(CollectingSink::new());
    let mut builder = AgentClient::with_transport(transport.clone()).diagnostic_sink(sink.clone());
    builder = builder.peer_version(peer_pin(fixture));
    let client = match builder.build() {
        Ok(client) => client,
        Err(e) => {
            observed.outcome = Outcome::Harness(format!(
                "harness: client build failed: {}",
                render_error(&e)
            ));
            return observed;
        }
    };
    let mut request = RunRequest::new().messages(history);
    if let Some((thread, run)) = first_run_ids(fixture) {
        request = request.thread_id(thread).run_id(run);
    }
    let mut handle = client.run(request);
    let mut failed = None;
    loop {
        match handle.next().await {
            Some(Ok(update)) => observed
                .events
                .push(serde_json::to_value(&*update.event).unwrap_or(Value::Null)),
            Some(Err(e)) => {
                failed = Some(render_error(&e));
                break;
            }
            None => break,
        }
    }
    observed.outcome = failed.map_or(Outcome::Completed, Outcome::Failed);
    observed.messages = serde_json::to_value(handle.messages()).unwrap_or(Value::Null);
    observed.state = handle.state().clone();
    observed.request = transport.last_request();
    observed.warnings = sink.take().iter().map(render_warning).collect();
    observed
}

/// [`observe_client`] on a fresh current-thread tokio runtime.
///
/// # Panics
///
/// When the runtime cannot be built.
pub fn observe_client_blocking(fixture: &StreamFixture, delivery: Delivery) -> Observed {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| panic!("tokio runtime: {e}"));
    runtime.block_on(observe_client(fixture, delivery))
}

/// How a lane is judged.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct JudgeOptions {
    /// Expectation keys this lane cannot observe. They are reported as
    /// skipped, never as met. Only real limits belong here (the frames lane
    /// sends no request); a lane that *differs* uses an override instead.
    pub unobservable: Vec<&'static str>,
}

impl JudgeOptions {
    /// Options for a lane that observes everything.
    pub fn new() -> Self {
        Self::default()
    }

    /// Options for the frames lanes, which send no request.
    pub fn frames() -> Self {
        JudgeOptions {
            unobservable: vec!["request", "requestAbsentPaths"],
        }
    }
}

/// Compares an observation with a fixture's Rust-lane expectations (the base
/// `expect` with [`Overrides::load`] applied).
///
/// # Errors
///
/// Every mismatch, and every expectation key the oracle does not know
/// (fail closed: supporting a new key is a deliberate code change).
pub fn judge(fixture: &StreamFixture, observed: &Observed) -> Result<(), Vec<String>> {
    judge_with(fixture, observed, &Overrides::load(), &JudgeOptions::new()).map(|_| ())
}

/// [`judge`] with explicit overrides and lane options. On success returns the
/// keys skipped as unobservable.
///
/// # Errors
///
/// As [`judge`].
pub fn judge_with(
    fixture: &StreamFixture,
    observed: &Observed,
    overrides: &Overrides,
    options: &JudgeOptions,
) -> Result<Vec<String>, Vec<String>> {
    let expect = overrides.resolve(fixture)?;
    judge_expect(&expect, observed, options).map_err(|problems| {
        problems
            .into_iter()
            .map(|p| format!("{}: {p}", fixture.name))
            .collect()
    })
}

/// Judges one resolved `expect` map. On success returns the keys skipped as
/// unobservable.
///
/// # Errors
///
/// Every mismatch and every unknown or ill-typed key.
pub fn judge_expect(
    expect: &Map<String, Value>,
    observed: &Observed,
    options: &JudgeOptions,
) -> Result<Vec<String>, Vec<String>> {
    let mut problems = Vec::new();
    let mut skipped = Vec::new();
    let harness = if let Outcome::Harness(e) = &observed.outcome {
        problems.push(format!("harness error, fixture not exercised: {e}"));
        true
    } else {
        false
    };
    for (key, want) in expect {
        if !EXPECT_KEYS.contains(&key.as_str()) {
            problems.push(format!(
                "unknown expectation key {key:?}: the oracle fails closed; teach it the key"
            ));
            continue;
        }
        if options.unobservable.contains(&key.as_str()) {
            skipped.push(key.clone());
            continue;
        }
        if harness && matches!(key.as_str(), "outcome" | "errorContains") {
            continue;
        }
        if let Err(p) = check_key(key, want, observed) {
            problems.push(format!("{key}: {p}"));
        }
    }
    if problems.is_empty() {
        Ok(skipped)
    } else {
        Err(problems)
    }
}

fn string_list<'a>(want: &'a Value, key: &str) -> Result<Vec<&'a str>, String> {
    want.as_array()
        .and_then(|items| items.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
        .ok_or_else(|| format!("{key} must be an array of strings"))
}

fn check_key(key: &str, want: &Value, obs: &Observed) -> Result<(), String> {
    match key {
        "outcome" => match (want.as_str(), &obs.outcome) {
            (Some("completed"), Outcome::Completed) | (Some("failed"), Outcome::Failed(_)) => {
                Ok(())
            }
            (Some("completed"), Outcome::Failed(e)) => {
                Err(format!("expected completed, client rejected: {e}"))
            }
            (Some("failed"), Outcome::Completed) => {
                Err("expected a client rejection, the stream was consumed".to_owned())
            }
            (Some("completed" | "failed"), Outcome::Harness(e)) => {
                Err(format!("harness error, fixture not exercised: {e}"))
            }
            _ => Err(format!("invalid value {want}")),
        },
        "errorContains" => {
            let needle = want.as_str().ok_or("must be a string")?;
            match &obs.outcome {
                Outcome::Harness(e) => Err(format!("harness error, fixture not exercised: {e}")),
                Outcome::Failed(e) if e.contains(needle) => Ok(()),
                Outcome::Failed(e) => Err(format!("error {e:?} does not contain {needle:?}")),
                Outcome::Completed => Err(format!("no error (wanted one containing {needle:?})")),
            }
        }
        "runError" => {
            let messages: Vec<&str> = obs
                .events
                .iter()
                .filter(|e| e.get("type").and_then(Value::as_str) == Some("RUN_ERROR"))
                .map(|e| e.get("message").and_then(Value::as_str).unwrap_or(""))
                .collect();
            match want {
                Value::Bool(true) if !messages.is_empty() => Ok(()),
                Value::Bool(true) => Err("no RUN_ERROR was delivered".to_owned()),
                Value::Bool(false) if messages.is_empty() => Ok(()),
                Value::Bool(false) => Err(format!("RUN_ERROR delivered: {messages:?}")),
                Value::String(needle) if messages.iter().any(|m| m.contains(needle.as_str())) => {
                    Ok(())
                }
                Value::String(needle) => Err(format!(
                    "no delivered RUN_ERROR message contains {needle:?} (got {messages:?})"
                )),
                _ => Err(format!("invalid value {want}")),
            }
        }
        "eventTypes" => {
            let want = string_list(want, key)?;
            let got = obs.delivered_types();
            if want == got {
                Ok(())
            } else {
                Err(format!("expected {want:?}, delivered {got:?}"))
            }
        }
        "eventTypesAbsent" => {
            let want = string_list(want, key)?;
            let got = obs.delivered_types();
            let present: Vec<&&str> = want.iter().filter(|t| got.contains(t)).collect();
            if present.is_empty() {
                Ok(())
            } else {
                Err(format!("delivered {present:?}"))
            }
        }
        "eventPaths" => {
            let map = want.as_object().ok_or("must be an object")?;
            let mut errs = Vec::new();
            for (path, value) in map {
                match indexed_lookup(&obs.events, path) {
                    Ok(Some(got)) if got == value => {}
                    Ok(Some(got)) => errs.push(format!("{path}: expected {value}, got {got}")),
                    Ok(None) => errs.push(format!("{path}: absent (expected {value})")),
                    Err(e) => errs.push(e),
                }
            }
            joined(errs)
        }
        "eventAbsentPaths" => absent_indexed(&obs.events, want, key),
        "messageAbsentPaths" => {
            let list = obs
                .messages
                .as_array()
                .ok_or("final messages are not an array")?;
            absent_indexed(list, want, key)
        }
        "warnings" => {
            let want = string_list(want, key)?;
            let missing: Vec<&&str> = want
                .iter()
                .filter(|needle| !obs.warnings.iter().any(|w| w.contains(**needle)))
                .collect();
            if missing.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "no warning contains {missing:?} (warnings: {:?})",
                    obs.warnings
                ))
            }
        }
        "noWarnings" => match want {
            Value::Bool(true) if obs.warnings.is_empty() => Ok(()),
            Value::Bool(true) => Err(format!("warnings were emitted: {:?}", obs.warnings)),
            Value::Bool(false) => Ok(()),
            _ => Err(format!("invalid value {want}")),
        },
        "messageCount" => {
            let want = want.as_u64().ok_or("must be a non-negative integer")?;
            let got = obs.messages.as_array().map_or(0, Vec::len) as u64;
            if want == got {
                Ok(())
            } else {
                Err(format!("expected {want}, client holds {got}"))
            }
        }
        "messages" => joined(subset(want, &obs.messages, "")),
        "state" => joined(subset(want, &obs.state, "")),
        "request" => match &obs.request {
            Some(request) => joined(subset(want, request, "")),
            None => Err("no request was observed".to_owned()),
        },
        "requestAbsentPaths" => {
            let request = obs.request.as_ref().ok_or("no request was observed")?;
            let paths = string_list(want, key)?;
            let present: Vec<&str> = paths
                .into_iter()
                .filter(|p| lookup(request, p).is_some())
                .collect();
            if present.is_empty() {
                Ok(())
            } else {
                Err(format!("present: {present:?}"))
            }
        }
        _ => Err("unreachable: key listed but not judged".to_owned()),
    }
}

fn joined(errs: Vec<String>) -> Result<(), String> {
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

/// Resolves a dotted path (`a.0.b`) inside `value`. Numeric segments index
/// arrays; on objects every segment is a member name.
pub fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }
    path.split('.')
        .try_fold(value, |current, segment| match current {
            Value::Object(map) => map.get(segment),
            Value::Array(items) => segment.parse::<usize>().ok().and_then(|i| items.get(i)),
            _ => None,
        })
}

/// Resolves `"<index>.<dotted path>"` in a list. `Ok(None)` when the path is
/// absent inside an existing element.
///
/// # Errors
///
/// A malformed key, or an index past the end of the list.
pub fn indexed_lookup<'a>(list: &'a [Value], key: &str) -> Result<Option<&'a Value>, String> {
    let (index, rest) = key.split_once('.').unwrap_or((key, ""));
    let index: usize = index
        .parse()
        .map_err(|_| format!("{key}: does not start with an index"))?;
    let item = list
        .get(index)
        .ok_or_else(|| format!("{key}: only {} item(s)", list.len()))?;
    Ok(lookup(item, rest))
}

fn absent_indexed(list: &[Value], want: &Value, key: &str) -> Result<(), String> {
    let mut errs = Vec::new();
    for path in string_list(want, key)? {
        match indexed_lookup(list, path) {
            Ok(None) => {}
            Ok(Some(got)) => errs.push(format!("{path}: present ({got})")),
            Err(e) => errs.push(e),
        }
    }
    joined(errs)
}

/// Subset match: objects compare only the expected members, arrays must have
/// the same length and match element-wise, everything else must be equal.
pub fn subset(want: &Value, got: &Value, at: &str) -> Vec<String> {
    let here = if at.is_empty() { "/" } else { at };
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => w
            .iter()
            .flat_map(|(k, wv)| match g.get(k) {
                Some(gv) => subset(wv, gv, &format!("{at}/{k}")),
                None => vec![format!("{at}/{k}: absent (expected {wv})")],
            })
            .collect(),
        (Value::Array(w), Value::Array(g)) if w.len() == g.len() => w
            .iter()
            .zip(g)
            .enumerate()
            .flat_map(|(i, (wv, gv))| subset(wv, gv, &format!("{at}/{i}")))
            .collect(),
        (Value::Array(w), Value::Array(g)) => {
            vec![format!(
                "{here}: expected {} element(s), got {}",
                w.len(),
                g.len()
            )]
        }
        _ if want == got => Vec::new(),
        _ => vec![format!("{here}: expected {want}, got {got}")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn expect(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn observed() -> Observed {
        let mut o = Observed::new();
        o.events = vec![
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
            json!({"type": "RUN_ERROR", "message": "boom: the agent failed", "code": "X"}),
            json!({"type": "STATE_SNAPSHOT", "snapshot": null}),
        ];
        o.warnings =
            vec!["processing.unknown_property TEXT_MESSAGE_CONTENT /futureProp stripped".into()];
        o.messages = json!([{"id": "m1", "role": "user", "content": "hi"}]);
        o.state = json!({"plan": {"status": "open", "steps": ["a", "b"]}});
        o.request = Some(json!({"messages": [{"id": "m1"}], "threadId": "t"}));
        o
    }

    fn ok(e: Value) {
        if let Err(p) = judge_expect(&expect(e.clone()), &observed(), &JudgeOptions::new()) {
            panic!("{e} should pass: {p:?}");
        }
    }

    fn fails(e: Value, needle: &str) {
        match judge_expect(&expect(e.clone()), &observed(), &JudgeOptions::new()) {
            Ok(_) => panic!("{e} should fail"),
            Err(p) => assert!(p.iter().any(|l| l.contains(needle)), "{e}: {p:?}"),
        }
    }

    #[test]
    fn harness_error_does_not_satisfy_failed() {
        let fixture = StreamFixture {
            name: "unparsable-input".to_owned(),
            path: std::path::PathBuf::new(),
            document: json!({
                "stream": [],
                "input": {"messages": [{"role": 7}]},
                "expect": {"outcome": "failed"}
            }),
        };
        let observed = observe(&fixture, Way::Json);
        for want in [
            json!({"outcome": "failed"}),
            json!({"errorContains": "harness"}),
            json!({"events": []}),
            json!({"state": {}}),
        ] {
            match judge_expect(&expect(want.clone()), &observed, &JudgeOptions::new()) {
                Ok(_) => panic!("{want} must not be met by a harness error"),
                Err(p) => assert!(
                    p.iter().any(|l| l.contains("harness error")),
                    "{want}: {p:?}"
                ),
            }
        }
    }

    #[test]
    fn every_key_passes_on_a_matching_observation() {
        ok(json!({
            "outcome": "completed",
            "runError": "the agent failed",
            "eventTypes": ["RUN_STARTED", "RUN_ERROR", "STATE_SNAPSHOT"],
            "eventTypesAbsent": ["FUTURE_EVENT"],
            "eventPaths": {"1.code": "X", "2.snapshot": null},
            "eventAbsentPaths": ["0.parentRunId", "2.snapshot.x"],
            "warnings": ["/futureProp", "TEXT_MESSAGE_CONTENT"],
            "noWarnings": false,
            "messageCount": 1,
            "messages": [{"role": "user"}],
            "messageAbsentPaths": ["0.name"],
            "state": {"plan": {"steps": ["a", "b"]}},
            "request": {"threadId": "t", "messages": [{}]},
            "requestAbsentPaths": ["protocolVersion", "messages.0.name"]
        }));
        ok(json!({"runError": true, "warnings": []}));
    }

    #[test]
    fn unknown_keys_fail_closed() {
        fails(
            json!({"outcome": "completed", "eventCount": 3}),
            "unknown expectation key \"eventCount\"",
        );
    }

    #[test]
    fn each_key_catches_its_mismatch() {
        fails(json!({"outcome": "failed"}), "expected a client rejection");
        fails(json!({"errorContains": "role"}), "no error");
        fails(json!({"runError": "unreachable"}), "no delivered RUN_ERROR");
        fails(json!({"runError": false}), "RUN_ERROR delivered");
        fails(json!({"eventTypes": ["RUN_STARTED"]}), "eventTypes");
        fails(json!({"eventTypesAbsent": ["RUN_ERROR"]}), "delivered");
        fails(
            json!({"eventPaths": {"2.snapshot": 0}}),
            "expected 0, got null",
        );
        fails(json!({"eventPaths": {"0.missing": null}}), "absent");
        fails(json!({"eventPaths": {"9.type": "X"}}), "only 3 item(s)");
        fails(json!({"eventAbsentPaths": ["1.code"]}), "present");
        fails(json!({"warnings": ["/role"]}), "no warning contains");
        fails(json!({"noWarnings": true}), "warnings were emitted");
        fails(json!({"messageCount": 2}), "client holds 1");
        fails(json!({"messages": []}), "expected 0 element(s), got 1");
        fails(json!({"messages": [{"role": "assistant"}]}), "/0/role");
        fails(json!({"messageAbsentPaths": ["0.content"]}), "present");
        fails(
            json!({"state": {"plan": {"steps": ["a"]}}}),
            "expected 1 element(s)",
        );
        fails(json!({"state": {"other": 1}}), "/other: absent");
        fails(json!({"request": {"protocolVersion": "1.0"}}), "absent");
        fails(json!({"requestAbsentPaths": ["threadId"]}), "present");
        fails(json!({"noWarnings": "yes"}), "invalid value");
    }

    #[test]
    fn rejection_text_is_matched() {
        let mut o = observed();
        o.outcome = Outcome::Failed("verify.text_message: role mismatch".into());
        let e = expect(json!({"outcome": "failed", "errorContains": "role"}));
        assert!(judge_expect(&e, &o, &JudgeOptions::new()).is_ok());
        let e = expect(json!({"errorContains": "delta"}));
        assert!(judge_expect(&e, &o, &JudgeOptions::new()).is_err());
    }

    #[test]
    fn unobservable_keys_are_skipped_not_met() {
        let mut o = observed();
        o.request = None;
        let e = expect(json!({"request": {"x": 1}, "outcome": "completed"}));
        assert!(judge_expect(&e, &o, &JudgeOptions::new()).is_err());
        let skipped = judge_expect(&e, &o, &JudgeOptions::frames()).unwrap_or_default();
        assert_eq!(skipped, vec!["request".to_owned()]);
    }

    #[test]
    fn subset_rules() {
        assert!(subset(
            &json!({"a": [1, {"b": 2}]}),
            &json!({"a": [1, {"b": 2, "c": 3}], "d": 4}),
            ""
        )
        .is_empty());
        assert!(!subset(&json!(["only-this"]), &json!({"0": "only-this"}), "").is_empty());
        assert!(!subset(&json!({}), &json!([]), "").is_empty());
        assert!(subset(&json!({}), &json!({"anything": true}), "").is_empty());
    }
}
