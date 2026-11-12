//! `proptest` strategies for AG-UI events and streams.
//!
//! Events are generated as wire JSON (`serde_json::Value`), the form every
//! pipeline entry point accepts, so the strategies do not depend on the typed
//! constructors and can also produce what a conformant producer never would.
//!
//! * [`event`]: any one of the 31 event types, shaped as the schema requires
//!   but with no ordering guarantees.
//! * [`conformant_stream`]: one or more complete runs, each a sequence of
//!   self-contained blocks (text messages, tool calls, steps, reasoning,
//!   state, custom, raw), so every generated stream is legal.
//! * [`chunk_stream`]: a run built from `*_CHUNK` events mixed with explicit
//!   ones, intended for a chunk-expansion property (none uses it yet).
//! * [`with_unknown_member`]: adds a member the schema does not define.
//! * [`split_points`], [`rechunk`]: arbitrary re-chunking of a byte buffer.
//! * [`json_value`], [`patch_ops`], [`patch_case`]: open JSON and RFC 6902
//!   operations, intended for patch properties (none uses them yet).

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;
use serde_json::{json, Map, Value};

/// A short identifier: `<prefix>-<n>`.
pub fn id(prefix: &'static str) -> impl Strategy<Value = String> {
    (0u32..10_000).prop_map(move |n| format!("{prefix}-{n}"))
}

/// Arbitrary text, including the empty string, newlines and non-ASCII.
pub fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[a-zA-Z0-9 ,.!?]{0,40}",
        1 => any::<String>().prop_map(|s| s.chars().take(40).collect()),
        1 => Just("line one\nline two\r\n\u{1F600} done".to_owned()),
    ]
}

/// Arbitrary JSON up to `depth` levels of nesting.
pub fn json_value(depth: u32) -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        (-1.0e9f64..1.0e9)
            .prop_map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)),
        "[a-z0-9 ]{0,12}".prop_map(Value::String),
    ];
    leaf.prop_recursive(depth, 64, 6, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..6).prop_map(Value::Array),
            vec(("[a-z]{1,6}", inner), 0..6)
                .prop_map(|members| Value::Object(members.into_iter().collect())),
        ]
    })
    .boxed()
}

/// A JSON object (open-object positions: state, metadata, content).
pub fn json_object(depth: u32) -> impl Strategy<Value = Value> {
    vec(("[a-z]{1,6}", json_value(depth)), 0..6)
        .prop_map(|members| Value::Object(members.into_iter().collect::<Map<_, _>>()))
}

/// A text-message role.
pub fn text_role() -> impl Strategy<Value = &'static str> {
    select(&["developer", "system", "assistant", "user"][..])
}

/// An RFC 6901 pointer into a small document: `""`, `/a`, `/a/0`, `/a/-`.
pub fn pointer() -> impl Strategy<Value = String> {
    vec(
        prop_oneof![
            "[a-c]",
            "[0-2]",
            Just("-".to_owned()),
            Just("~1x".to_owned())
        ],
        0..4,
    )
    .prop_map(|tokens| tokens.iter().map(|t| format!("/{t}")).collect())
}

/// One RFC 6902 operation (well-formed; it may or may not apply).
pub fn patch_op() -> impl Strategy<Value = Value> {
    prop_oneof![
        (pointer(), json_value(2)).prop_map(|(p, v)| json!({"op": "add", "path": p, "value": v})),
        pointer().prop_map(|p| json!({"op": "remove", "path": p})),
        (pointer(), json_value(2))
            .prop_map(|(p, v)| json!({"op": "replace", "path": p, "value": v})),
        (pointer(), pointer()).prop_map(|(f, p)| json!({"op": "move", "from": f, "path": p})),
        (pointer(), pointer()).prop_map(|(f, p)| json!({"op": "copy", "from": f, "path": p})),
        (pointer(), json_value(1)).prop_map(|(p, v)| json!({"op": "test", "path": p, "value": v})),
    ]
}

/// A patch: up to `max` operations.
pub fn patch_ops(max: usize) -> impl Strategy<Value = Vec<Value>> {
    vec(patch_op(), 0..=max)
}

/// A document over the keys [`pointer()`] uses, and a patch against it.
pub fn patch_case() -> impl Strategy<Value = (Value, Vec<Value>)> {
    let doc = vec(("[a-c]", json_value(3)), 0..4)
        .prop_map(|members| Value::Object(members.into_iter().collect()));
    (doc, patch_ops(8))
}

fn ev(kind: &str, fields: Value) -> Value {
    let mut map = Map::new();
    map.insert("type".to_owned(), Value::String(kind.to_owned()));
    if let Value::Object(fields) = fields {
        map.extend(fields);
    }
    Value::Object(map)
}

/// Any single event, schema-shaped, of any of the 31 types. Sequences of
/// these are generally not legal streams; see [`conformant_stream`].
pub fn event() -> BoxedStrategy<Value> {
    let m = || id("m");
    let tc = || id("tc");
    prop_oneof![
        (id("t"), id("r")).prop_map(|(t, r)| ev("RUN_STARTED", json!({"threadId": t, "runId": r}))),
        (id("t"), id("r"))
            .prop_map(|(t, r)| ev("RUN_FINISHED", json!({"threadId": t, "runId": r}))),
        text().prop_map(|msg| ev("RUN_ERROR", json!({"message": msg}))),
        id("step").prop_map(|s| ev("STEP_STARTED", json!({"stepName": s}))),
        id("step").prop_map(|s| ev("STEP_FINISHED", json!({"stepName": s}))),
        (m(), text_role()).prop_map(|(id, role)| ev(
            "TEXT_MESSAGE_START",
            json!({"messageId": id, "role": role})
        )),
        (m(), text())
            .prop_map(|(id, d)| ev("TEXT_MESSAGE_CONTENT", json!({"messageId": id, "delta": d}))),
        m().prop_map(|id| ev("TEXT_MESSAGE_END", json!({"messageId": id}))),
        (m(), text())
            .prop_map(|(id, d)| ev("TEXT_MESSAGE_CHUNK", json!({"messageId": id, "delta": d}))),
        (tc(), "[a-z_]{1,10}").prop_map(|(id, n)| ev(
            "TOOL_CALL_START",
            json!({"toolCallId": id, "toolCallName": n})
        )),
        (tc(), text())
            .prop_map(|(id, d)| ev("TOOL_CALL_ARGS", json!({"toolCallId": id, "delta": d}))),
        tc().prop_map(|id| ev("TOOL_CALL_END", json!({"toolCallId": id}))),
        (tc(), text())
            .prop_map(|(id, d)| ev("TOOL_CALL_CHUNK", json!({"toolCallId": id, "delta": d}))),
        (m(), tc(), text()).prop_map(|(m, t, c)| ev(
            "TOOL_CALL_RESULT",
            json!({"messageId": m, "toolCallId": t, "content": c})
        )),
        json_value(3).prop_map(|s| ev("STATE_SNAPSHOT", json!({"snapshot": s}))),
        patch_ops(4).prop_map(|d| ev("STATE_DELTA", json!({"delta": d}))),
        (m(), text()).prop_map(|(id, c)| ev(
            "MESSAGES_SNAPSHOT",
            json!({"messages": [{"id": id, "role": "user", "content": c}]})
        )),
        (m(), json_object(2)).prop_map(|(id, c)| ev(
            "ACTIVITY_SNAPSHOT",
            json!({"messageId": id, "activityType": "plan", "content": c})
        )),
        (m(), patch_ops(3)).prop_map(|(id, p)| ev(
            "ACTIVITY_DELTA",
            json!({"messageId": id, "activityType": "plan", "patch": p})
        )),
        json_value(2).prop_map(|e| ev("RAW", json!({"event": e}))),
        ("[a-z.]{1,12}", json_value(2))
            .prop_map(|(n, v)| ev("CUSTOM", json!({"name": n, "value": v}))),
        m().prop_map(|id| ev("REASONING_START", json!({"messageId": id}))),
        m().prop_map(|id| ev(
            "REASONING_MESSAGE_START",
            json!({"messageId": id, "role": "reasoning"})
        )),
        (m(), text()).prop_map(|(id, d)| ev(
            "REASONING_MESSAGE_CONTENT",
            json!({"messageId": id, "delta": d})
        )),
        m().prop_map(|id| ev("REASONING_MESSAGE_END", json!({"messageId": id}))),
        (m(), text()).prop_map(|(id, d)| ev(
            "REASONING_MESSAGE_CHUNK",
            json!({"messageId": id, "delta": d})
        )),
        m().prop_map(|id| ev("REASONING_END", json!({"messageId": id}))),
        (m(), "[A-Za-z0-9+/=]{4,24}").prop_map(|(id, v)| ev(
            "REASONING_ENCRYPTED_VALUE",
            json!({"subtype": "message", "entityId": id, "encryptedValue": v})
        )),
        (id("s"), "[a-z]{1,8}")
            .prop_map(|(s, n)| ev("SUBAGENT_STARTED", json!({"subagentRunId": s, "name": n}))),
        id("s").prop_map(|s| ev("SUBAGENT_FINISHED", json!({"subagentRunId": s}))),
        (id("s"), text()).prop_map(|(s, msg)| ev(
            "SUBAGENT_ERROR",
            json!({"subagentRunId": s, "message": msg})
        )),
    ]
    .boxed()
}

/// One self-contained, legal block inside a run, with ids unique to
/// `(run, n)`: the verifier keys identities per thread, so ids must not repeat
/// across the runs of one thread.
fn block(run: &str, n: usize) -> BoxedStrategy<Vec<Value>> {
    let mid = format!("m-{run}-{n}");
    let tid = format!("tc-{run}-{n}");
    let step = format!("step-{run}-{n}");
    prop_oneof![
        // Text message triad.
        (text_role(), vec(text(), 0..4)).prop_map({
            let mid = mid.clone();
            move |(role, deltas)| {
                let mut out = vec![ev("TEXT_MESSAGE_START", json!({"messageId": mid, "role": role}))];
                out.extend(deltas.into_iter().map(|d| ev("TEXT_MESSAGE_CONTENT", json!({"messageId": mid, "delta": d}))));
                out.push(ev("TEXT_MESSAGE_END", json!({"messageId": mid})));
                out
            }
        }),
        // Tool call with arguments, optionally with its result.
        ("[a-z_]{1,10}", vec("[a-z0-9{}\":,]{0,8}", 0..4), any::<bool>()).prop_map({
            let (mid, tid) = (mid.clone(), tid.clone());
            move |(name, args, with_result)| {
                let mut out = vec![ev("TOOL_CALL_START", json!({"toolCallId": tid, "toolCallName": name}))];
                out.extend(args.into_iter().map(|d| ev("TOOL_CALL_ARGS", json!({"toolCallId": tid, "delta": d}))));
                out.push(ev("TOOL_CALL_END", json!({"toolCallId": tid})));
                if with_result {
                    out.push(ev("TOOL_CALL_RESULT", json!({"messageId": format!("{mid}-result"), "toolCallId": tid, "content": "ok"})));
                }
                out
            }
        }),
        // Step pair.
        Just(vec![
            ev("STEP_STARTED", json!({"stepName": step})),
            ev("STEP_FINISHED", json!({"stepName": step})),
        ]),
        // Reasoning span with one message.
        vec(text(), 0..3).prop_map({
            let mid = mid.clone();
            move |deltas| {
                let rid = format!("{mid}-r");
                let mut out = vec![
                    ev("REASONING_START", json!({"messageId": rid})),
                    ev("REASONING_MESSAGE_START", json!({"messageId": mid, "role": "reasoning"})),
                ];
                out.extend(deltas.into_iter().map(|d| ev("REASONING_MESSAGE_CONTENT", json!({"messageId": mid, "delta": d}))));
                out.push(ev("REASONING_MESSAGE_END", json!({"messageId": mid})));
                out.push(ev("REASONING_END", json!({"messageId": rid})));
                out
            }
        }),
        // Whole-state replacement.
        json_value(3).prop_map(|s| vec![ev("STATE_SNAPSHOT", json!({"snapshot": s}))]),
        // Passthrough.
        ("[a-z.]{1,12}", json_value(2)).prop_map(|(n, v)| vec![ev("CUSTOM", json!({"name": n, "value": v}))]),
        json_value(2).prop_map(|e| vec![ev("RAW", json!({"event": e}))]),
    ]
    .boxed()
}

/// One complete run: `RUN_STARTED`, up to `max_blocks` legal blocks,
/// `RUN_FINISHED`.
pub fn conformant_run(
    thread: String,
    run: String,
    max_blocks: usize,
) -> impl Strategy<Value = Vec<Value>> {
    let block_run = run.clone();
    (0..=max_blocks)
        .prop_flat_map(move |n| (0..n).map(|i| block(&block_run, i)).collect::<Vec<_>>())
        .prop_map(move |blocks| {
            let mut out = vec![ev("RUN_STARTED", json!({"threadId": thread, "runId": run}))];
            out.extend(blocks.into_iter().flatten());
            out.push(ev(
                "RUN_FINISHED",
                json!({"threadId": thread, "runId": run}),
            ));
            out
        })
}

/// One to three sequential complete runs on one thread.
pub fn conformant_stream() -> impl Strategy<Value = Vec<Value>> {
    (1usize..=3)
        .prop_flat_map(|runs| {
            (0..runs)
                .map(|i| conformant_run("t-prop".to_owned(), format!("r-prop-{i}"), 6))
                .collect::<Vec<_>>()
        })
        .prop_map(|runs| runs.into_iter().flatten().collect())
}

/// A run whose body mixes `TEXT_MESSAGE_CHUNK` / `TOOL_CALL_CHUNK` events with
/// explicit ones, in arbitrary order. A stream may or may not be legal: the
/// intended property is that expansion either produces a stream the verifier
/// accepts or fails with a `verify.chunk` error.
pub fn chunk_stream() -> impl Strategy<Value = Vec<Value>> {
    let body = prop_oneof![
        (0u8..3, proptest::option::of(text())).prop_map(|(m, d)| {
            let mut f = json!({"messageId": format!("m-c{m}")});
            if let Some(d) = d {
                f["delta"] = Value::String(d);
            }
            ev("TEXT_MESSAGE_CHUNK", f)
        }),
        (
            0u8..3,
            proptest::option::of("[a-z_]{1,6}"),
            proptest::option::of(text())
        )
            .prop_map(|(t, n, d)| {
                let mut f = json!({"toolCallId": format!("tc-c{t}")});
                if let Some(n) = n {
                    f["toolCallName"] = Value::String(n);
                }
                if let Some(d) = d {
                    f["delta"] = Value::String(d);
                }
                ev("TOOL_CALL_CHUNK", f)
            }),
        (0u8..3).prop_map(|m| ev(
            "TEXT_MESSAGE_START",
            json!({"messageId": format!("m-c{m}")})
        )),
        (0u8..3).prop_map(|m| ev("TEXT_MESSAGE_END", json!({"messageId": format!("m-c{m}")}))),
        (0u8..3).prop_map(|t| ev("TOOL_CALL_END", json!({"toolCallId": format!("tc-c{t}")}))),
        ("[a-z]{1,6}").prop_map(|n| ev("CUSTOM", json!({"name": n, "value": null}))),
    ];
    vec(body, 0..16).prop_map(|events| {
        let mut out = vec![ev(
            "RUN_STARTED",
            json!({"threadId": "t-chunk", "runId": "r-chunk"}),
        )];
        out.extend(events);
        out.push(ev(
            "RUN_FINISHED",
            json!({"threadId": "t-chunk", "runId": "r-chunk"}),
        ));
        out
    })
}

/// Adds one member the schema does not define (an unknown property that
/// enforcement must strip with a warning).
pub fn with_unknown_member(event: impl Strategy<Value = Value>) -> impl Strategy<Value = Value> {
    (event, "zz[a-z]{1,6}", json_value(1)).prop_map(|(mut e, k, v)| {
        if let Value::Object(map) = &mut e {
            map.insert(k, v);
        }
        e
    })
}

/// Sorted, distinct split offsets strictly inside `0..len`.
pub fn split_points(len: usize) -> impl Strategy<Value = Vec<usize>> {
    vec(1..len.max(2), 0..16).prop_map(move |mut points| {
        points.retain(|p| *p < len);
        points.sort_unstable();
        points.dedup();
        points
    })
}

/// Splits `bytes` at `points` (as produced by [`split_points`]).
pub fn split_at(bytes: &[u8], points: &[usize]) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(points.len() + 1);
    let mut start = 0;
    for &p in points {
        if p > start && p < bytes.len() {
            out.push(bytes[start..p].to_vec());
            start = p;
        }
    }
    out.push(bytes[start..].to_vec());
    out
}

/// `bytes` re-chunked at arbitrary offsets.
pub fn rechunk(bytes: Vec<u8>) -> impl Strategy<Value = Vec<Vec<u8>>> {
    split_points(bytes.len()).prop_map(move |points| split_at(&bytes, &points))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    fn sample<S: Strategy>(s: S) -> Vec<S::Value> {
        let mut runner = TestRunner::deterministic();
        (0..64)
            .map(|_| {
                s.new_tree(&mut runner)
                    .unwrap_or_else(|e| panic!("{e}"))
                    .current()
            })
            .collect()
    }

    #[test]
    fn every_event_has_a_type_and_all_types_occur() {
        let mut seen = std::collections::BTreeSet::new();
        let mut runner = TestRunner::deterministic();
        for _ in 0..2000 {
            let e = event()
                .new_tree(&mut runner)
                .unwrap_or_else(|e| panic!("{e}"))
                .current();
            seen.insert(e["type"].as_str().unwrap_or_default().to_owned());
        }
        assert_eq!(seen.len(), 31, "{seen:?}");
    }

    #[test]
    fn conformant_streams_are_bracketed() {
        for stream in sample(conformant_stream()) {
            assert_eq!(stream[0]["type"], "RUN_STARTED");
            assert_eq!(stream[stream.len() - 1]["type"], "RUN_FINISHED");
            let starts = stream.iter().filter(|e| e["type"] == "RUN_STARTED").count();
            let ends = stream
                .iter()
                .filter(|e| e["type"] == "RUN_FINISHED")
                .count();
            assert_eq!(starts, ends);
        }
    }

    #[test]
    fn conformant_streams_are_accepted_by_the_pipeline() {
        use agui_sdk::diagnostics::{NullSink, Reporter};
        use agui_sdk::pipeline::{PipelineConfig, Processor, Termination};
        use std::sync::Arc;

        let mut runner = TestRunner::deterministic();
        let strategy = conformant_stream();
        for case in 0..256 {
            let stream = strategy
                .new_tree(&mut runner)
                .unwrap_or_else(|e| panic!("{e}"))
                .current();
            let mut processor = Processor::new(PipelineConfig::default());
            let mut reporter = Reporter::new(Arc::new(NullSink), 0);
            let mut out = Vec::new();
            for event in &stream {
                let result = processor.push_value(event.clone(), &mut reporter, &mut out);
                reporter.end_event(event.get("type").and_then(Value::as_str));
                if let Err(e) = result {
                    panic!("case {case}: pipeline refused {event}: {e}\n{stream:#?}");
                }
            }
            assert_ne!(processor.finish(), Termination::Truncated, "case {case}");
        }
    }

    proptest! {
        #[test]
        fn rechunking_preserves_bytes(bytes in vec(any::<u8>(), 0..200), points in split_points(200)) {
            let pieces = split_at(&bytes, &points);
            prop_assert_eq!(pieces.concat(), bytes);
        }
    }
}
