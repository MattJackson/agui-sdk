//! Regressions for the second security audit (hostile peers): resource
//! exhaustion and limit bypasses a peer can trigger with small inputs. Each
//! test fails on the code the audit ran against.
//!
//! Allocations are counted per thread (`agui_test_support::alloc`), so a
//! test's byte budget sees only what its own measured closure allocated, not
//! the tests running beside it (the L2 frames below build several MB). The
//! tests that measure still serialise with a lock, so the timing ones (M2)
//! do not compete for the CPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agui_sdk::diagnostics::NullSink;
use agui_sdk::diagnostics::{CollectingSink, Diagnostic, DiagnosticCode, Reporter, Scope};
use agui_sdk::pipeline::compat::{Boundary, EraShims};
use agui_sdk::pipeline::verify::ConsumerVerifier;
use agui_sdk::pipeline::{Limits, PipelineConfig, Processor};
use agui_sdk::version::LegacyEra;
use agui_sdk::{ErrorCode, Event};
use agui_test_support::alloc::{self, CountingAllocator};
use serde_json::{json, Value};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

static SERIAL: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `f`, returning its result and the bytes it allocated on this thread
/// (reallocation growth included): an upper bound on its peak.
fn allocated<T>(f: impl FnOnce() -> T) -> (T, usize) {
    assert!(
        alloc::installed(),
        "the counting allocator is not installed"
    );
    let (out, stats) = alloc::measure(f);
    (out, usize::try_from(stats.bytes).unwrap_or(usize::MAX))
}

fn push(
    p: &mut Processor,
    reporter: &mut Reporter,
    event: &Value,
) -> agui_sdk::Result<Vec<agui_sdk::pipeline::Processed>> {
    let mut out = Vec::new();
    p.push_json(event.to_string().as_bytes(), reporter, &mut out)?;
    Ok(out)
}

/// Node units: one per value, plus one per 64 bytes of string or key text.
fn nodes(v: &Value) -> usize {
    1 + match v {
        Value::Array(a) => a.iter().map(nodes).sum(),
        Value::Object(m) => m.iter().map(|(k, x)| k.len() / 64 + nodes(x)).sum(),
        Value::String(s) => s.len() / 64,
        _ => 0,
    }
}

#[cfg(feature = "client")]
mod transport_support;

// ---------------------------------------------------------------- protobuf

/// proto-1: a protobuf frame's container entries are budgeted while it is
/// decoded, not only afterwards: a run of 2-byte empty patch operations is
/// about 370 times its size once materialised.
#[cfg(all(feature = "proto", feature = "client"))]
mod entry_budget {
    use super::*;
    use agui_sdk::client::transport::HttpTransport;
    use agui_sdk::client::{AgentClient, RunRequest};
    use std::future::IntoFuture;
    use transport_support::{block, events, proto, MockServer, Reply};

    #[test]
    fn a_frame_of_empty_patch_operations_is_refused_before_it_is_materialised() {
        let _g = lock();
        let mut body = proto(&events()[..1]);
        let delta = [0x12u8, 0x00].repeat(100_000);
        let message = [
            vec![8 << 3 | 2],
            {
                let mut v = Vec::new();
                let mut n = delta.len();
                while n >= 0x80 {
                    v.push((n as u8) | 0x80);
                    n >>= 7;
                }
                v.push(n as u8);
                v
            },
            delta.clone(),
        ]
        .concat();
        agui_sdk::proto::framing::write_frame(&message, &mut body).unwrap();
        let frame_len = message.len();
        let mut limits = Limits::default();
        limits.max_container_entries = 1_000;

        let (result, bytes) = allocated(|| {
            block(async {
                let server = MockServer::always(Reply::proto(body))
                    .await
                    .unwrap_or_else(|e| panic!("{e}"));
                let transport = HttpTransport::builder(server.url("/")).build().unwrap();
                let client = AgentClient::with_transport(transport)
                    .limits(limits)
                    .build()
                    .unwrap();
                client
                    .run(RunRequest::new().thread_id("t1").run_id("r1").user("hi"))
                    .into_future()
                    .await
                    .map(|_| ())
            })
        });
        assert_eq!(result.unwrap_err().code(), ErrorCode::LimitExceeded);
        assert!(
            bytes < 32 * frame_len,
            "the run allocated {bytes} bytes for a {frame_len}-byte frame"
        );
    }
}

#[cfg(feature = "proto")]
mod proto {
    use super::*;
    use agui_sdk::proto::decode::{decode_event, decode_event_json, Decoded};

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    fn ld(field: u32, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 12);
        varint(u64::from(field) << 3 | 2, &mut out);
        varint(body.len() as u64, &mut out);
        out.extend_from_slice(body);
        out
    }

    /// `Event{STATE_SNAPSHOT{snapshot: v}}`.
    fn snapshot(v: &[u8]) -> Vec<u8> {
        ld(7, &ld(2, v))
    }

    /// `levels` of `{"a": …}` around a string leaf. With `merge`, every level
    /// repeats its message fields (an empty occurrence first), which protobuf
    /// merges: `Value{5: <>, 5: Struct{1: Entry{1: "a", 2: <>, 2: next}}}`.
    fn chain(levels: usize, leaf: &[u8], merge: bool) -> Vec<u8> {
        let mut v = ld(3, leaf);
        for _ in 0..levels {
            let mut entry = ld(1, b"a");
            if merge {
                entry.extend(ld(2, &[]));
            }
            entry.extend(ld(2, &v));
            let mut next = Vec::new();
            if merge {
                next.extend(ld(5, &[]));
            }
            next.extend(ld(5, &ld(1, &entry)));
            v = next;
        }
        snapshot(&v)
    }

    /// H1: merged message occurrences are read where they lie. Before the
    /// fix every level copied the rest of the frame twice (about 200 MB for
    /// this 1 MiB frame); now decoding allocates about the output.
    #[test]
    fn h1_merged_fields_are_decoded_without_copying_per_level() {
        let _g = lock();
        let leaf = vec![b'x'; 1 << 20];
        let frame = chain(100, &leaf, true);
        let (decoded, bytes) = allocated(|| decode_event(&frame));
        let Decoded::Event(v) = decoded.unwrap() else {
            panic!("not an event")
        };
        let mut at = &v["snapshot"];
        for _ in 0..100 {
            at = &at["a"];
        }
        assert_eq!(at.as_str().map(str::len), Some(1 << 20));
        assert!(
            bytes < 3 * frame.len(),
            "decode_event allocated {bytes} bytes for a {}-byte frame",
            frame.len()
        );
        let mut text = String::new();
        let (r, bytes) = allocated(|| decode_event_json(&frame, &mut text));
        r.unwrap();
        assert!(
            bytes < 3 * frame.len(),
            "decode_event_json allocated {bytes} bytes for a {}-byte frame",
            frame.len()
        );
    }

    /// L2: hostile nesting is refused at the pipeline's depth, cleanly, on a
    /// tokio-sized (2 MiB) stack. Before the fix the decoder recursed to 512
    /// levels and a debug build aborted on stack overflow.
    #[test]
    fn l2_deep_nesting_fails_cleanly_on_a_2_mib_stack() {
        let _g = lock();
        let frame = chain(510, b"v", false);
        let code = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || decode_event(&frame).map(|_| ()).map_err(|e| e.code()))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(code, Err(ErrorCode::LimitExceeded));
    }

    /// L2: the decoder accepts exactly the nesting enforcement accepts.
    #[test]
    fn l2_decode_depth_matches_enforcement() {
        let _g = lock();
        let deep = |n: usize| {
            format!(
                r#"{{"type":"CUSTOM","name":"n","value":{}1{}}}"#,
                "[".repeat(n),
                "]".repeat(n)
            )
        };
        // Enforcement counts the event object, so `max_depth` levels of
        // arrays inside it are one too many.
        let max = Limits::default().max_depth;
        for (n, ok) in [(max - 1, true), (max, false)] {
            let text = deep(n);
            let mut bin = Vec::new();
            agui_sdk::proto::encode::encode_json(text.as_bytes(), &mut bin).unwrap();
            let decoded = decode_event(&bin).map(|_| ()).map_err(|e| e.code());
            let sink = Arc::new(CollectingSink::new());
            let mut reporter = Reporter::new(sink, 256);
            let enforced = agui_sdk::pipeline::enforce::Enforcer::new(Limits::default())
                .enforce_event_json(text.as_bytes(), &mut reporter)
                .map(|_| ())
                .map_err(|e| e.code());
            assert_eq!(decoded.is_ok(), ok, "decode at {n}: {decoded:?}");
            assert_eq!(enforced.is_ok(), ok, "enforce at {n}: {enforced:?}");
        }
    }
}

// ---------------------------------------------------------------- patches

/// An object of `n` members whose keys all have length `len + 8` and share a
/// prefix, so any key comparison runs the full prefix.
#[cfg(feature = "preserve-order")]
fn long_key_object(n: usize, len: usize) -> Value {
    let prefix = "x".repeat(len);
    Value::Object(
        (0..n)
            .map(|i| (format!("{prefix}{i:08}"), json!(i)))
            .collect(),
    )
}

/// `remove` of an object member under `preserve-order` found its position by
/// comparing key strings, so one removal cost members x key length bytes
/// while the walk budget charged members / 16. The position is now found by
/// identity: 8x the members and 8x the key length take about 8x the time (it
/// was 64x). The bound is `LINEAR_8X`: 4x slack over linear, as the other
/// wall-clock checks keep.
#[cfg(feature = "preserve-order")]
#[test]
fn patch_remove_position_scan_does_not_compare_key_bytes() {
    use agui_sdk::patch::{self, JsonPointer, PatchBudget, PatchOperation};
    let _g = lock();
    let case = |k: usize| {
        let doc = long_key_object(1024 * k, 256 * k);
        let last: Vec<String> = doc
            .as_object()
            .unwrap()
            .keys()
            .rev()
            .take(256)
            .cloned()
            .collect();
        let ops: Vec<PatchOperation> = last
            .iter()
            .map(|key| PatchOperation::remove(JsonPointer::new_unchecked(format!("/{key}"))))
            .collect();
        (doc, ops)
    };
    let time = |k: usize| {
        let (doc, ops) = case(k);
        best_of_three(&doc, |d| {
            let mut budget = PatchBudget::for_document(d, Default::default());
            patch::apply(d, &ops, &mut budget).unwrap();
        })
    };
    let (t1, t8) = (time(1), time(8));
    assert!(
        t8 < t1 * LINEAR_8X,
        "8x members and key length took {t8:?} against {t1:?}"
    );
    // The right members were removed, in order, and a failing patch restores
    // the original order.
    let (mut doc, mut ops) = case(1);
    ops.push(PatchOperation::test(
        JsonPointer::new_unchecked(""),
        json!(0),
    ));
    let before = doc.clone();
    let mut budget = PatchBudget::for_document(&doc, Default::default());
    assert!(patch::apply(&mut doc, &ops, &mut budget).is_err());
    assert!(
        doc.as_object()
            .unwrap()
            .keys()
            .eq(before.as_object().unwrap().keys()),
        "member order changed by a failed patch"
    );
}

/// `diff` counted the whole `after` subtree at every object level that had
/// to decide between member-wise removals and a rebuild, then recursed into
/// it and counted again: O(depth x size). The count stops once the answer is
/// known, so 8x the depth takes about 8x the time (it was 64x), bounded by
/// `LINEAR_8X`.
#[cfg(feature = "preserve-order")]
#[test]
fn diff_removal_cost_check_is_linear_in_depth() {
    let _g = lock();
    const W: usize = 256;
    let shape = |depth: usize, keep_from: usize| -> Value {
        let mut v = json!({});
        for _ in 0..depth {
            let mut level: serde_json::Map<String, Value> = (keep_from..W)
                .map(|i| (format!("m{i}"), json!(i)))
                .collect();
            level.insert("pad".into(), json!(vec![0; 2000]));
            level.insert("next".into(), v);
            v = Value::Object(level);
        }
        v
    };
    let ops_for = |depth: usize| {
        let (before, after) = (shape(depth, 0), shape(depth, 20));
        (before, after)
    };
    let time = |depth: usize| {
        let (before, after) = ops_for(depth);
        let mut best = Duration::MAX;
        for _ in 0..3 {
            let started = Instant::now();
            std::hint::black_box(agui_sdk::patch::diff(&before, &after));
            best = best.min(started.elapsed());
        }
        best
    };
    let (t1, t8) = (time(20), time(160));
    assert!(
        t8 < t1 * LINEAR_8X,
        "8x the depth took {t8:?} against {t1:?}"
    );
    let (before, after) = ops_for(20);
    let patch = agui_sdk::patch::diff(&before, &after);
    // 20 removals at each of the 20 levels: the decision is unchanged.
    assert_eq!(patch.len(), 400);
    let mut doc = before.clone();
    let mut budget = agui_sdk::patch::PatchBudget::for_document(&doc, Default::default());
    agui_sdk::patch::apply(&mut doc, &patch, &mut budget).unwrap();
    assert_eq!(doc, after);
}

/// H2: a 70-byte `STATE_DELTA` doubled the state every time, with no
/// absolute cap: the growth allowance compounded. Now the state stops at
/// `PatchLimits::max_nodes` (180,000 by default) and the stream fails
/// with `processing.limit`, after a `patch.budget_exceeded` diagnostic.
#[test]
fn h2_state_growth_stops_at_a_fatal_absolute_ceiling() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 256);
    let mut p = Processor::new(PipelineConfig::default());
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
    )
    .unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "STATE_SNAPSHOT", "snapshot": {"s": 1}}),
    )
    .unwrap();
    let copies: Vec<Value> = (0..16)
        .map(|k| json!({"op": "copy", "from": "", "path": format!("/k{k}")}))
        .collect();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "STATE_DELTA", "delta": copies}),
    )
    .unwrap();
    assert_eq!(nodes(p.reducer().state()), 1 << 17);
    let ceiling = Limits::default().patch.max_nodes;
    let mut failed = None;
    // 2^18 > 180,000.
    for k in 16..18 {
        let delta = json!({"type": "STATE_DELTA", "delta": [
            {"op": "copy", "from": "", "path": format!("/k{k}")}
        ]});
        match push(&mut p, &mut reporter, &delta) {
            Ok(_) => assert!(nodes(p.reducer().state()) <= ceiling),
            Err(e) => {
                failed = Some((k, e.code()));
                break;
            }
        }
    }
    assert_eq!(failed, Some((16, ErrorCode::LimitExceeded)));
    assert_eq!(
        nodes(p.reducer().state()),
        1 << 17,
        "the refused patch is not applied"
    );
    assert!(
        sink.take()
            .iter()
            .any(|d| d.code == DiagnosticCode::PatchBudgetExceeded),
        "the refusal is reported"
    );
}

/// A `copy` is a few bytes on the wire and duplicates a resident string: it
/// is charged by the string's length and refused before it is allocated.
/// With no growth allowance the first copy is already refused, so the
/// refused frame must allocate nothing like the 1 MiB string.
#[test]
fn h2_copy_of_a_large_string_is_refused_without_allocating_it() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 256);
    let mut config = PipelineConfig::default();
    config.limits.patch.growth_factor = 1;
    config.limits.patch.min_nodes = 1;
    let mut p = Processor::new(config);
    push(&mut p, &mut reporter, &started()).unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "STATE_SNAPSHOT", "snapshot": {"s": "x".repeat(1 << 20)}}),
    )
    .unwrap();
    let before = p.reducer().state().clone();
    sink.take();
    let copies: Vec<Value> = (0..200)
        .map(|k| json!({"op": "copy", "from": "/s", "path": format!("/c{k}")}))
        .collect();
    let frame = json!({"type": "STATE_DELTA", "delta": copies});
    let (r, bytes) = allocated(|| push(&mut p, &mut reporter, &frame));
    r.unwrap();
    assert!(
        sink.take()
            .iter()
            .any(|d| d.code == DiagnosticCode::PatchBudgetExceeded),
        "the patch is refused"
    );
    assert!(p.reducer().state() == &before, "state is unchanged");
    assert!(
        bytes < 1 << 20,
        "allocated {} KiB for a refused patch",
        bytes >> 10
    );
}

fn small_ceiling() -> PipelineConfig {
    let mut config = PipelineConfig::default();
    config.limits.patch.max_nodes = 100_000;
    config
}

/// A `copy` refused at the ceiling by a long member key is refused before
/// the key is cloned: the refused frame allocates nothing like the key.
#[test]
fn h2_copy_refused_by_a_long_key_does_not_clone_it() {
    let _g = lock();
    let mut reporter = Reporter::new(Arc::new(CollectingSink::new()), 256);
    let mut p = Processor::new(small_ceiling());
    push(&mut p, &mut reporter, &started()).unwrap();
    let key = "k".repeat(4 << 20);
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "STATE_SNAPSHOT", "snapshot": {"o": {key: 0}}}),
    )
    .unwrap();
    let frame = json!({"type": "STATE_DELTA", "delta": [
        {"op": "copy", "from": "/o", "path": "/c"}
    ]});
    let frame = frame.to_string();
    let (r, bytes) = allocated(|| {
        let mut out = Vec::new();
        p.push_json(frame.as_bytes(), &mut reporter, &mut out)
    });
    assert_eq!(r.unwrap_err().code(), ErrorCode::LimitExceeded);
    assert!(
        bytes < 1 << 20,
        "allocated {} KiB for a copy refused by its key",
        bytes >> 10
    );
}

/// Repeated small frames that each copy a long string accumulate its bytes
/// in the state: they reach the ceiling and end the stream.
#[test]
fn h2_repeated_small_string_copies_reach_the_ceiling() {
    let _g = lock();
    let mut reporter = Reporter::new(Arc::new(CollectingSink::new()), 256);
    let mut p = Processor::new(small_ceiling());
    push(&mut p, &mut reporter, &started()).unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "STATE_SNAPSHOT", "snapshot": {"s": "x".repeat(64 * 1024)}}),
    )
    .unwrap();
    let mut failed = None;
    for k in 0..100 {
        let delta = json!({"type": "STATE_DELTA", "delta": [
            {"op": "copy", "from": "/s", "path": format!("/c{k}")}
        ]});
        match push(&mut p, &mut reporter, &delta) {
            Ok(_) => assert!(nodes(p.reducer().state()) <= 100_000),
            Err(e) => {
                failed = Some(e.code());
                break;
            }
        }
    }
    assert_eq!(failed, Some(ErrorCode::LimitExceeded));
}

/// The same for `ACTIVITY_DELTA`, with the history byte limit off.
#[test]
fn h2_repeated_activity_string_copies_reach_the_ceiling() {
    let _g = lock();
    let mut reporter = Reporter::new(Arc::new(CollectingSink::new()), 256);
    let mut config = small_ceiling();
    config.limits.max_history_bytes = usize::MAX;
    let mut p = Processor::new(config);
    push(&mut p, &mut reporter, &started()).unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
            "content": {"s": "x".repeat(64 * 1024)}}),
    )
    .unwrap();
    let mut failed = None;
    for k in 0..100 {
        let delta = json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t",
            "patch": [{"op": "copy", "from": "/s", "path": format!("/c{k}")}]});
        if let Err(e) = push(&mut p, &mut reporter, &delta) {
            failed = Some(e.code());
            break;
        }
    }
    assert_eq!(failed, Some(ErrorCode::LimitExceeded));
}

/// H2: the ceiling is configurable through `Limits::patch`, and it covers
/// the state and every patched activity content together, so many small
/// documents cannot multiply it.
#[test]
fn h2_ceiling_is_configurable_and_shared_across_documents() {
    let _g = lock();
    let mut limits = Limits::default();
    limits.patch.max_nodes = 10_000;
    let mut config = PipelineConfig::default();
    config.limits = limits;
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(config);
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
    )
    .unwrap();
    let mut failed = None;
    'grow: for doc in 0..10 {
        let id = format!("a{doc}");
        push(
            &mut p,
            &mut reporter,
            &json!({"type": "ACTIVITY_SNAPSHOT", "messageId": id, "activityType": "x",
                    "content": {"s": 1}}),
        )
        .unwrap();
        for k in 0..12 {
            let delta = json!({"type": "ACTIVITY_DELTA", "messageId": id, "activityType": "x",
                "patch": [{"op": "copy", "from": "", "path": format!("/k{k}")}]});
            if let Err(e) = push(&mut p, &mut reporter, &delta) {
                failed = Some((doc, e.code()));
                break 'grow;
            }
        }
    }
    // The first document reaches 2^13 = 8192 nodes; the second, alone far
    // under the ceiling, would take the total past 10,000, so the stream
    // ends while growing it.
    assert_eq!(failed, Some((1, ErrorCode::LimitExceeded)));
    let total: usize = p
        .reducer()
        .messages()
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .map(|m| nodes(&m["content"]))
        .sum();
    assert!(total <= 10_000, "{total}");
}

// ---------------------------------------------------------------- verifier

/// M2: `STEP_STARTED` with distinct owners cost a scan of every open owner
/// (O(N²) for N events). Indexed now: 4x the events take about 4x the time,
/// not 16x.
#[test]
fn m2_step_owner_tracking_is_linear() {
    let _g = lock();
    let events = |n: usize| -> Vec<Event> {
        let mut v = vec![serde_json::from_value::<Event>(
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
        )
        .unwrap()];
        v.extend((0..n).map(|i| {
            serde_json::from_value::<Event>(json!({
                "type": "STEP_STARTED", "stepName": "s", "subagentRunId": format!("sub-{i}")
            }))
            .unwrap()
        }));
        v
    };
    let time = |events: &[Event]| -> Duration {
        (0..3)
            .map(|_| {
                let mut verifier = ConsumerVerifier::new();
                let started = Instant::now();
                for e in events {
                    verifier.observe(e).unwrap();
                }
                started.elapsed()
            })
            .min()
            .unwrap()
    };
    let (small, large) = (events(5_000), events(80_000));
    let (t_small, t_large) = (time(&small), time(&large));
    assert!(
        t_large < t_small * LINEAR_16X,
        "16x the events took {:.1}x the time ({t_small:?} -> {t_large:?})",
        t_large.as_secs_f64() / t_small.as_secs_f64()
    );
}

/// A resume list of N unknown ids cost O(N²) comparisons to report (an
/// `position` scan per entry, outside the reporter cap). One pass now: 4x the
/// entries take about 4x the time.
#[cfg(feature = "server")]
#[test]
fn resume_unrecognised_reporting_is_linear() {
    use agui_sdk::diagnostics::NullSink;
    use agui_sdk::interrupt::ResumeEntry;
    use agui_sdk::server::helpers::ResumeCheck;

    let _g = lock();
    let entries = |n: usize| -> Vec<ResumeEntry> {
        let prefix = "p".repeat(200);
        (0..n)
            .map(|i| ResumeEntry::cancelled(format!("{prefix}{i}")))
            .collect()
    };
    let time = |entries: &[ResumeEntry]| -> Duration {
        (0..3)
            .map(|_| {
                let mut reporter = Reporter::new(Arc::new(NullSink), 0);
                let started = Instant::now();
                let check = ResumeCheck::classify(&[], entries);
                check.report_unrecognised(entries, &mut reporter);
                started.elapsed()
            })
            .min()
            .unwrap()
    };
    let (small, large) = (entries(5_000), entries(80_000));
    let (t_small, t_large) = (time(&small), time(&large));
    assert!(
        t_large < t_small * LINEAR_16X,
        "16x the entries took {:.1}x the time ({t_small:?} -> {t_large:?})",
        t_large.as_secs_f64() / t_small.as_secs_f64()
    );
}

// ---------------------------------------------------------------- boundary

/// L3: a frame routed through the compatibility boundary (any frame whose
/// bytes contain `THINKING_`) was parsed into a full tree before the
/// container-entry budget applied. The budget now applies while parsing.
#[test]
fn l3_boundary_frames_are_budgeted_while_parsing() {
    let _g = lock();
    let mut limits = Limits::default();
    limits.max_container_entries = 1_000;
    let mut config = PipelineConfig::default();
    config.limits = limits;
    let zeros = vec!["0"; 500_000].join(",");
    let frame =
        format!(r#"{{"type":"STATE_SNAPSHOT","snapshot":{{"THINKING_":0,"a":[{zeros}]}}}}"#);
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(config);
    let mut out = Vec::new();
    let (r, bytes) = allocated(|| p.push_json(frame.as_bytes(), &mut reporter, &mut out));
    assert_eq!(r.unwrap_err().code(), ErrorCode::LimitExceeded);
    assert!(
        bytes < frame.len(),
        "{bytes} bytes allocated refusing a {}-byte frame",
        frame.len()
    );
}

/// Best-of-three wall time of `run` over a fresh clone of `input` each time.
fn best_of_three<T: Clone>(input: &T, mut run: impl FnMut(&mut T)) -> Duration {
    (0..3)
        .map(|_| {
            let mut copy = input.clone();
            let started = Instant::now();
            run(&mut copy);
            started.elapsed()
        })
        .min()
        .unwrap()
}

/// Legacy `binary` parts were checked against every modern part (O(N^2)) and
/// removed one by one. The mirror check is hashed now: 4x the parts take
/// about 4x the time, not 16x.
#[test]
fn boundary_legacy_mirror_check_is_linear() {
    let _g = lock();
    let frame = |n: usize| -> Value {
        let mut parts: Vec<Value> = (0..n)
            .map(|i| json!({"type": "binary", "mimeType": "image/png", "url": format!("u-{i}")}))
            .collect();
        parts.extend((0..n).map(|i| {
            json!({"type": "image",
                   "source": {"type": "url", "value": format!("v-{i}"), "mimeType": "image/png"}})
        }));
        json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c", "content": parts})
    };
    let time = |n: usize| {
        let input = frame(n);
        let mut reporter = Reporter::new(Arc::new(NullSink), 0);
        best_of_three(&input, |f| {
            assert!(Boundary::new().upgrade(f, &mut reporter));
            assert_eq!(f["content"].as_array().unwrap().len(), 2 * n);
        })
    };
    let (t_small, t_large) = (time(1_000), time(16_000));
    assert!(
        t_large < t_small * LINEAR_16X,
        "16x the parts took {:.1}x the time ({t_small:?} -> {t_large:?})",
        t_large.as_secs_f64() / t_small.as_secs_f64()
    );
}

/// Out-of-shape (object) filenames sharing one source were compared with a
/// linear scan of the bucket per upgraded part (O(N^2)). Hashed structurally
/// now: 4x the parts take about 4x the time.
#[test]
fn boundary_legacy_mirror_check_with_object_filenames_is_linear() {
    let _g = lock();
    let frame = |n: usize| -> Value {
        let mut parts: Vec<Value> = (0..n)
            .map(|i| {
                json!({"type": "binary", "mimeType": "image/png", "url": "u",
                       "metadata": {"filename": {"k": format!("L{i}")}}})
            })
            .collect();
        parts.extend((0..n).map(|i| {
            json!({"type": "image",
                   "source": {"type": "url", "value": "u", "mimeType": "image/png"},
                   "metadata": {"filename": {"k": format!("M{i}")}}})
        }));
        json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c", "content": parts})
    };
    let time = |n: usize| {
        let input = frame(n);
        let mut reporter = Reporter::new(Arc::new(NullSink), 0);
        best_of_three(&input, |f| {
            assert!(Boundary::new().upgrade(f, &mut reporter));
            assert_eq!(f["content"].as_array().unwrap().len(), 2 * n);
        })
    };
    let (t_small, t_large) = (time(1_000), time(16_000));
    assert!(
        t_large < t_small * LINEAR_16X,
        "16x the parts took {:.1}x the time ({t_small:?} -> {t_large:?})",
        t_large.as_secs_f64() / t_small.as_secs_f64()
    );
}

/// Retired `THINKING_*` spans were tracked in a Vec (linear `rposition` and
/// `remove` per END). Indexed now: 4x the spans take about 4x the time.
#[test]
fn thinking_span_tracking_is_linear() {
    let _g = lock();
    let frames = |n: usize| -> Vec<Value> {
        let mut v = vec![json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"})];
        v.extend(
            (0..n).map(|i| json!({"type": "THINKING_START", "messageId": format!("span-{i}")})),
        );
        v.extend((0..n).map(|i| json!({"type": "THINKING_END", "messageId": format!("span-{i}")})));
        v
    };
    let time = |n: usize| {
        let input = frames(n);
        let mut reporter = Reporter::new(Arc::new(NullSink), 0);
        best_of_three(&input, |fs| {
            let mut boundary = Boundary::new();
            for f in fs.iter_mut() {
                assert!(boundary.upgrade(f, &mut reporter));
            }
        })
    };
    let (t_small, t_large) = (time(10_000), time(160_000));
    assert!(
        t_large < t_small * LINEAR_16X,
        "16x the spans took {:.1}x the time ({t_small:?} -> {t_large:?})",
        t_large.as_secs_f64() / t_small.as_secs_f64()
    );
}

/// A capped reporter builds nothing: a flood of lossy compat warnings costs
/// no allocation per warning.
#[test]
fn capped_compat_warnings_allocate_nothing_per_warning() {
    let _g = lock();
    let parts: Vec<Value> = (0..64)
        .map(|_| json!({"type": "image", "source": {"type": "url", "value": "u"}}))
        .collect();
    let mut frame = json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
                           "content": parts});
    let shims = EraShims::new(LegacyEra::V0_0_47);
    let mut reporter = Reporter::new(Arc::new(NullSink), 0);
    let (_, stats) = alloc::measure(|| shims.downgrade_event(&mut frame, &mut reporter));
    let allocations = stats.allocations;
    assert_eq!(frame["content"].as_array().unwrap().len(), 0);
    assert!(
        allocations < 8,
        "{allocations} allocations for 64 capped warnings"
    );
}

// ---------------------------------------------------------------- diagnostics

/// L4: peer strings in diagnostics are bounded previews: the pointer (and
/// each member name in it) and the unknown event type, as well as the
/// message.
#[test]
fn l4_diagnostics_carry_only_bounded_previews_of_peer_strings() {
    let _g = lock();
    let long = "A".repeat(1_000_000);
    let d = Diagnostic::new(
        DiagnosticCode::UnknownField,
        Scope::Event,
        format!("/{long}"),
        "x",
    )
    .with_unknown_type(long.as_str());
    assert!(d.pointer.len() <= 260, "{}", d.pointer.len());
    assert!(d.unknown_type.as_deref().map_or(0, str::len) <= 70);
    assert!(d.to_string().len() < 1_000);

    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 256);
    let mut p = Processor::new(PipelineConfig::default());
    let secret = format!("sk-live-{}", "S".repeat(200));
    for event in [
        json!({"type": long}),
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
        json!({"type": "STEP_STARTED", "stepName": "s", (secret.clone()): 1}),
        json!({"type": "STEP_STARTED", "stepName": "u", (long.clone()): 1}),
    ] {
        push(&mut p, &mut reporter, &event).unwrap();
    }
    let seen = sink.take();
    assert!(seen.len() >= 3, "{seen:?}");
    for d in &seen {
        let text = d.to_string();
        assert!(text.len() < 1_000, "{} bytes: {}", text.len(), &text[..200]);
        assert!(!text.contains(&secret), "member name verbatim: {text}");
        assert!(d.pointer.len() <= 260);
    }
    let dropped = seen
        .iter()
        .find(|d| d.code == DiagnosticCode::UnknownEvent)
        .expect("the unknown event's diagnostic");
    assert_eq!(
        dropped.unknown_type, d.unknown_type,
        "the same bounded preview"
    );
    assert!(dropped.message.contains(&*d.unknown_type.clone().unwrap()));
}

// ---------------------------------------------------------------- reducer

fn started() -> Value {
    json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"})
}

/// Frames as the bytes `push_json` takes.
fn frames(events: &[Value]) -> Vec<Vec<u8>> {
    events.iter().map(|e| e.to_string().into_bytes()).collect()
}

/// Pushes every frame through a fresh default processor; the elapsed time.
fn time_frames(frames: &[Vec<u8>]) -> Duration {
    (0..3)
        .map(|_| {
            let sink = Arc::new(CollectingSink::new());
            let mut reporter = Reporter::new(sink, 256);
            let mut p = Processor::new(PipelineConfig::default());
            let mut out = Vec::new();
            let started = Instant::now();
            for f in frames {
                out.clear();
                p.push_json(f, &mut reporter, &mut out).unwrap();
            }
            started.elapsed()
        })
        .min()
        .unwrap()
}

/// Time bound for 16x the input in the wall-clock linearity checks below:
/// 4x slack over linear (64x), while a quadratic path takes 256x. At 4x the
/// input linear and quadratic were only 4x vs 16x apart, and stalls on shared
/// debug-build runners (these run in every test lane) crossed any threshold
/// between them; 16x leaves a factor of 4 on both sides.
const LINEAR_16X: u32 = 64;

/// Time bound for 8x the input (the `preserve-order` checks above, whose
/// inputs are too large to scale to 16x): 4x slack over linear (32x), still
/// half of the 64x a quadratic path takes. Tighter bounds (2x slack) were
/// crossed by runner stalls on linear code.
#[cfg(feature = "preserve-order")]
const LINEAR_8X: u32 = 32;

fn assert_linear(small: &[Vec<u8>], large: &[Vec<u8>]) {
    let (t_small, t_large) = (time_frames(small), time_frames(large));
    assert!(
        t_large < t_small * LINEAR_16X,
        "16x the events took {:.1}x the time ({t_small:?} -> {t_large:?})",
        t_large.as_secs_f64() / t_small.as_secs_f64()
    );
}

/// R189/R221: reopening N calls of one message under a new parent cost
/// O(N) each (quadratic in all). A reopen keeps the call where it is.
#[test]
fn reopen_with_new_parent_is_linear() {
    let _g = lock();
    let events = |n: usize| -> Vec<Vec<u8>> {
        let mut v = vec![started()];
        for parent in ["A", "B"] {
            for i in 0..n {
                v.push(
                    json!({"type": "TOOL_CALL_START", "toolCallId": format!("c{i}"),
                    "toolCallName": "f", "parentMessageId": parent}),
                );
                v.push(json!({"type": "TOOL_CALL_END", "toolCallId": format!("c{i}")}));
            }
        }
        frames(&v)
    };
    assert_linear(&events(3_000), &events(48_000));
}

fn activity_snapshots(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            json!({"type": "ACTIVITY_SNAPSHOT", "messageId": format!("a{i}"),
            "activityType": "t", "content": {}})
        })
        .collect()
}

/// R190/R222: every `MESSAGES_SNAPSHOT` rebuilt the whole index and
/// re-cloned every held id, so a tiny snapshot cost O(held history).
#[test]
fn tiny_snapshots_do_not_scale_with_held_history() {
    let _g = lock();
    let measure = |n: usize| -> usize {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink, 256);
        let mut p = Processor::new(PipelineConfig::default());
        push(&mut p, &mut reporter, &started()).unwrap();
        for e in activity_snapshots(n) {
            push(&mut p, &mut reporter, &e).unwrap();
        }
        let frame = json!({"type": "MESSAGES_SNAPSHOT", "messages": []})
            .to_string()
            .into_bytes();
        let mut out = Vec::new();
        let (r, bytes) = allocated(|| p.push_json(&frame, &mut reporter, &mut out));
        r.unwrap();
        bytes
    };
    let (small, large) = (measure(1_000), measure(8_000));
    assert!(
        large <= small + 4_096,
        "an empty snapshot allocated {small} bytes against 1k held messages and {large} against 8k"
    );
}

#[test]
fn messages_snapshot_against_held_activity_is_linear() {
    let _g = lock();
    let empty = || json!({"type": "MESSAGES_SNAPSHOT", "messages": []});
    let events = |n: usize| -> Vec<Vec<u8>> {
        let mut v = vec![started()];
        v.extend(activity_snapshots(n));
        v.extend((0..n).map(|_| empty()));
        frames(&v)
    };
    assert_linear(&events(2_500), &events(40_000));

    // Drop one message and append one, again and again.
    let churn = |n: usize| -> Vec<Vec<u8>> {
        let mut v = vec![started()];
        v.extend(activity_snapshots(n));
        v.extend((0..n).map(|k| {
            json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                {"id": format!("u{k}"), "role": "user", "content": "x"}]})
        }));
        frames(&v)
    };
    assert_linear(&churn(2_500), &churn(40_000));

    // A reducer-minted carrier clashing with a retained message must not
    // leave the snapshots expecting a drop that never comes.
    let clash = |n: usize| -> Vec<Vec<u8>> {
        let mut v = vec![
            started(),
            json!({"type": "REASONING_MESSAGE_START", "messageId": "c1", "role": "reasoning"}),
            json!({"type": "REASONING_MESSAGE_END", "messageId": "c1"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f"}),
            empty(),
        ];
        v.extend(activity_snapshots(n));
        v.extend((0..n).map(|_| empty()));
        frames(&v)
    };
    assert_linear(&clash(2_500), &clash(40_000));
}

/// One large `MESSAGES_SNAPSHOT` restating every held message (the bench's
/// `messages_snapshot/reconcile` shape): reconciling it is linear in the
/// messages. 32k messages stay under the default 150,000-entry frame budget.
#[test]
fn messages_snapshot_restating_every_message_is_linear() {
    let _g = lock();
    let events = |n: usize| -> Vec<Vec<u8>> {
        let messages: Vec<Value> = (0..n)
            .map(|k| json!({"id": format!("m{k}"), "role": "user", "content": "x"}))
            .collect();
        frames(&[
            started(),
            json!({"type": "MESSAGES_SNAPSHOT", "messages": messages.clone()}),
            json!({"type": "MESSAGES_SNAPSHOT", "messages": messages}),
        ])
    };
    assert_linear(&events(2_000), &events(32_000));
}

/// A snapshot that drops nothing keeps the activity budgets: retained
/// content still counts against the shared node ceiling.
#[test]
fn retained_activity_still_counts_against_the_node_ceiling_after_a_snapshot() {
    let _g = lock();
    let mut config = PipelineConfig::default();
    config.limits.patch.max_nodes = 100;
    config.limits.patch.min_nodes = 100;
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(config);
    let zeros: Vec<u8> = vec![0; 60];
    for e in [
        started(),
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
            "content": {"k": zeros}}),
        json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t",
            "patch": [{"op": "add", "path": "/z", "value": 1}]}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
        json!({"type": "STATE_SNAPSHOT", "snapshot": {}}),
    ] {
        push(&mut p, &mut reporter, &e).unwrap();
    }
    let big: Vec<u8> = vec![0; 50];
    let err = push(
        &mut p,
        &mut reporter,
        &json!({"type": "STATE_DELTA", "delta": [{"op": "add", "path": "/s", "value": big}]}),
    )
    .unwrap_err();
    assert_eq!(err.code(), ErrorCode::LimitExceeded);
}

// ---------------------------------------------------------------- history bound

fn history_limit(bytes: usize) -> PipelineConfig {
    let mut config = PipelineConfig::default();
    config.limits.max_history_bytes = bytes;
    config
}

fn code_of(r: agui_sdk::Result<Vec<agui_sdk::pipeline::Processed>>) -> Option<ErrorCode> {
    r.err().map(|e| e.code())
}

fn delta(id: &str, kib: usize) -> Value {
    json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": id, "delta": "x".repeat(kib * 1024)})
}

/// R203: text, arguments and message count grew without bound.
#[test]
fn history_growth_stops_at_a_fatal_limit() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
    )
    .unwrap();
    let mut failed_at = None;
    for i in 0..40 {
        if let Some(code) = code_of(push(&mut p, &mut reporter, &delta("m", 4))) {
            assert_eq!(code, ErrorCode::LimitExceeded);
            failed_at = Some(i);
            break;
        }
    }
    let at = failed_at.expect("history growth must hit the limit");
    assert!(at <= 17, "failed only at delta {at}");
    // The processor is spent.
    assert!(push(&mut p, &mut reporter, &delta("m", 1)).is_err());
}

#[test]
fn history_limit_covers_minted_messages() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    let mut failed = false;
    for e in activity_snapshots(5_000) {
        if let Some(code) = code_of(push(&mut p, &mut reporter, &e)) {
            assert_eq!(code, ErrorCode::LimitExceeded);
            failed = true;
            break;
        }
    }
    assert!(failed, "5,000 empty messages must exceed 64 KiB");
}

#[test]
fn history_limit_is_net_of_snapshot_drops() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    for (round, id) in ["m1", "m2", "m3"].into_iter().enumerate() {
        push(
            &mut p,
            &mut reporter,
            &json!({"type": "TEXT_MESSAGE_START", "messageId": id, "role": "assistant"}),
        )
        .unwrap();
        for _ in 0..12 {
            push(&mut p, &mut reporter, &delta(id, 4)).unwrap();
        }
        push(
            &mut p,
            &mut reporter,
            &json!({"type": "TEXT_MESSAGE_END", "messageId": id}),
        )
        .unwrap();
        // 48 KiB per round fits; three rounds do not, unless the drop is credited.
        push(
            &mut p,
            &mut reporter,
            &json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
        )
        .unwrap_or_else(|e| panic!("round {round}: {e}"));
    }
}

#[test]
fn restored_history_above_limit_still_streams() {
    let _g = lock();
    let history: Vec<agui_sdk::message::Message> = serde_json::from_value(json!([
        {"id": "old", "role": "user", "content": "y".repeat(128 * 1024)}
    ]))
    .unwrap();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::with_history(history_limit(64 * 1024), history, json!({}));
    for e in [
        started(),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
        delta("m", 1),
    ] {
        push(&mut p, &mut reporter, &e).unwrap();
    }
}

/// A `copy` operation is a few bytes on the wire and a deep clone in memory.
#[test]
fn history_limit_counts_patch_copies() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
            "content": {"s": "x".repeat(40 * 1024)}}),
    )
    .unwrap();
    let mut failed = false;
    for i in 0..8 {
        let r = push(
            &mut p,
            &mut reporter,
            &json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t",
                "patch": [{"op": "copy", "from": "/s", "path": format!("/s{i}")}]}),
        );
        if let Some(code) = code_of(r) {
            assert_eq!(code, ErrorCode::LimitExceeded);
            failed = true;
            break;
        }
    }
    assert!(failed, "copies of a 40 KiB value must exceed 64 KiB");
}

/// Pushes `events` and asserts the history limit fires within them.
fn assert_limit_within(
    p: &mut Processor,
    reporter: &mut Reporter,
    events: impl IntoIterator<Item = Value>,
    what: &str,
) -> usize {
    for (i, e) in events.into_iter().enumerate() {
        if let Some(code) = code_of(push(p, reporter, &e)) {
            assert_eq!(code, ErrorCode::LimitExceeded, "{what}");
            return i;
        }
    }
    panic!("{what}: the stream never ended with LimitExceeded");
}

/// What is dropped must be credited at what it was charged: escaped text
/// charged raw but credited at its serialized length banked headroom.
#[test]
fn history_limit_is_not_banked_by_escaped_text() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    for i in 0..20 {
        let id = format!("m{i}");
        for e in [
            json!({"type": "TEXT_MESSAGE_START", "messageId": id, "role": "assistant"}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": id,
                "delta": "\u{1}".repeat(4096)}),
            json!({"type": "TEXT_MESSAGE_END", "messageId": id}),
            json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
        ] {
            push(&mut p, &mut reporter, &e).unwrap();
        }
    }
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
    )
    .unwrap();
    let at = assert_limit_within(
        &mut p,
        &mut reporter,
        (0..40).map(|_| delta("m", 4)),
        "text",
    );
    assert!(at <= 17, "failed only at delta {at}");
}

#[test]
fn history_limit_is_not_banked_by_escaped_arguments() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    for i in 0..20 {
        let id = format!("c{i}");
        for e in [
            json!({"type": "TOOL_CALL_START", "toolCallId": id, "toolCallName": "f"}),
            json!({"type": "TOOL_CALL_ARGS", "toolCallId": id, "delta": "\"".repeat(4096)}),
            json!({"type": "TOOL_CALL_END", "toolCallId": id}),
            json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
        ] {
            push(&mut p, &mut reporter, &e).unwrap();
        }
    }
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
    )
    .unwrap();
    let at = assert_limit_within(
        &mut p,
        &mut reporter,
        (0..40).map(|_| delta("m", 4)),
        "args",
    );
    assert!(at <= 17, "failed only at delta {at}");
}

/// A copy sized after the patch lost its source to a later remove, while the
/// remove was credited in full: the mirror patch banked headroom.
#[test]
fn history_limit_survives_mirrored_copy_remove() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
            "content": {"a": "x".repeat(40 * 1024)}}),
    )
    .unwrap();
    for i in 0..100 {
        let (from, to) = if i % 2 == 0 {
            ("/a", "/b")
        } else {
            ("/b", "/a")
        };
        push(
            &mut p,
            &mut reporter,
            &json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t",
                "patch": [{"op": "copy", "from": from, "path": to},
                          {"op": "remove", "path": from}]}),
        )
        .unwrap_or_else(|e| panic!("patch {i}: {e}"));
    }
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
    )
    .unwrap();
    let at = assert_limit_within(
        &mut p,
        &mut reporter,
        (0..40).map(|_| delta("m", 4)),
        "mirror",
    );
    assert!(at <= 7, "failed only at delta {at}");
}

/// [copy, move the source away] charged the copy nothing.
#[test]
fn history_limit_counts_copy_then_move() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
            "content": {"a": "x".repeat(20 * 1024)}}),
    )
    .unwrap();
    // Each frame copies the value and moves the source on: the copy's source
    // is gone once the patch is applied.
    let frames = (0..40).map(|i| {
        let key = |n: usize| {
            if n == 0 {
                "/a".to_owned()
            } else {
                format!("/a{n}")
            }
        };
        json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t",
            "patch": [{"op": "copy", "from": key(i), "path": format!("/b{i}")},
                      {"op": "move", "from": key(i), "path": key(i + 1)}]})
    });
    let at = assert_limit_within(&mut p, &mut reporter, frames, "copy then move");
    assert!(at <= 4, "failed only at frame {at}");
}

#[test]
fn history_limit_counts_reopened_call_names() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    let big = "n".repeat(8 * 1024);
    let mut frames = Vec::new();
    for i in 0..40 {
        let id = format!("c{i}");
        frames.push(json!({"type": "TOOL_CALL_START", "toolCallId": id, "toolCallName": "n"}));
        frames.push(json!({"type": "TOOL_CALL_END", "toolCallId": id}));
        frames.push(json!({"type": "TOOL_CALL_START", "toolCallId": id, "toolCallName": big}));
    }
    let at = assert_limit_within(&mut p, &mut reporter, frames, "reopened names");
    assert!(at <= 9 * 3, "failed only at frame {at}");
}

#[test]
fn history_limit_counts_carrier_names() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    let big = "n".repeat(8 * 1024);
    let mut frames = Vec::new();
    for i in 0..40 {
        frames.push(
            json!({"type": "TOOL_CALL_START", "toolCallId": format!("c{i}"),
            "toolCallName": "f", "parentMessageId": format!("m{i}")}),
        );
        frames.push(
            json!({"type": "TEXT_MESSAGE_START", "messageId": format!("m{i}"),
            "role": "assistant", "name": big}),
        );
    }
    let at = assert_limit_within(&mut p, &mut reporter, frames, "carrier names");
    assert!(at <= 9 * 2, "failed only at frame {at}");
}

#[test]
fn history_limit_counts_activity_retypes() {
    let _g = lock();
    let big = "t".repeat(8 * 1024);
    for via_delta in [false, true] {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink, 256);
        let mut p = Processor::new(history_limit(64 * 1024));
        push(&mut p, &mut reporter, &started()).unwrap();
        let mut frames = Vec::new();
        for i in 0..40 {
            let id = format!("a{i}");
            frames.push(json!({"type": "ACTIVITY_SNAPSHOT", "messageId": id,
                "activityType": "t", "content": {}}));
            frames.push(if via_delta {
                json!({"type": "ACTIVITY_DELTA", "messageId": id, "activityType": big,
                    "patch": [{"op": "add", "path": "/k", "value": 1}]})
            } else {
                json!({"type": "ACTIVITY_SNAPSHOT", "messageId": id,
                    "activityType": big, "content": {}})
            });
        }
        let at = assert_limit_within(&mut p, &mut reporter, frames, "retypes");
        assert!(
            at <= 9 * 2,
            "via_delta={via_delta}: failed only at frame {at}"
        );
    }
    // Retyping one activity to the same type again and again is not growth.
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    for _ in 0..40 {
        push(
            &mut p,
            &mut reporter,
            &json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a",
                "activityType": big, "content": {}}),
        )
        .unwrap();
    }
}

/// A refreshed encrypted value replaces the old one: no phantom growth.
#[test]
fn history_limit_is_net_of_refreshed_encrypted_values() {
    let _g = lock();
    let big = "e".repeat(8 * 1024);
    for subtype in ["message", "tool_call"] {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink, 256);
        let mut p = Processor::new(history_limit(64 * 1024));
        push(&mut p, &mut reporter, &started()).unwrap();
        let entity = if subtype == "message" {
            push(
                &mut p,
                &mut reporter,
                &json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
            )
            .unwrap();
            "m"
        } else {
            push(
                &mut p,
                &mut reporter,
                &json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "f"}),
            )
            .unwrap();
            "c"
        };
        for i in 0..20 {
            push(
                &mut p,
                &mut reporter,
                &json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": subtype,
                    "entityId": entity, "encryptedValue": big}),
            )
            .unwrap_or_else(|e| panic!("{subtype} refresh {i}: {e}"));
        }
    }
}

/// Swapping a held activity into a user message and dropping it again, over
/// and over, walked the whole held tail each time: the stream ends instead.
#[test]
fn class_swap_drop_cycles_end_the_stream() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 256);
    // The work bound is independent of the history limit: it fires with the
    // limit disabled.
    let mut p = Processor::new(history_limit(usize::MAX));
    push(&mut p, &mut reporter, &started()).unwrap();
    let n = 6_000;
    for e in activity_snapshots(n) {
        push(&mut p, &mut reporter, &e).unwrap();
    }
    let frames = (0..n).flat_map(|i| {
        [
            json!({"type": "MESSAGES_SNAPSHOT", "messages":
                [{"id": format!("a{i}"), "role": "user", "content": ""}]}),
            json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
        ]
    });
    let at = assert_limit_within(&mut p, &mut reporter, frames, "class swaps");
    assert!(at < n, "ended only at frame {at}");
    assert!(
        sink.take()
            .iter()
            .any(|d| d.code == DiagnosticCode::SnapshotWorkBound),
        "the verdict must be the reconcile work bound"
    );
}

/// Security audit S1: history is charged what a held `serde_json` value
/// costs in memory, not its JSON text. `{"a":[0; 100_000]}` is 200 KB of
/// wire and about 3.2 MB held, so 32 MiB of history holds about ten such
/// activity contents, not about 168.
#[test]
fn history_limit_charges_resident_value_cost() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(32 * 1024 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    let zeros = vec![0; 100_000];
    let mut failed_at = None;
    for i in 0..200 {
        let frame = json!({
            "type": "ACTIVITY_SNAPSHOT", "messageId": format!("a{i}"),
            "activityType": "t", "content": {"a": zeros},
        });
        if let Some(code) = code_of(push(&mut p, &mut reporter, &frame)) {
            assert_eq!(code, ErrorCode::LimitExceeded);
            failed_at = Some(i);
            break;
        }
    }
    let at = failed_at.expect("history growth must hit the limit");
    assert!(at <= 11, "failed only at activity snapshot {at}");
}

#[test]
fn history_limit_default_is_128_mib() {
    assert_eq!(Limits::default().max_history_bytes, 128 * 1024 * 1024);
}

/// `{"a":{"a":…{"a":0}}}`, `depth` objects deep: one container entry and
/// one node unit per object, the costliest shape per unit.
fn chain(depth: usize) -> Value {
    (0..depth).fold(json!(0), |inner, _| json!({ "a": inner }))
}

/// Security audit S2: a `STATE_SNAPSHOT` is bounded by the default entry
/// limit (150,000), so one in-limits frame of single-member objects cannot
/// install the hundreds of MiB the old 1,000,000-entry default allowed.
#[test]
fn state_snapshot_bounded_by_entry_limit() {
    let _g = lock();
    // Chains in an array under `{"c": …}`: an element and `depth` members
    // per chain, plus the outer member.
    let run = |chains: usize, depth: usize| {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink, 256);
        let mut p = Processor::new(PipelineConfig::default());
        push(&mut p, &mut reporter, &started()).unwrap();
        let snapshot = json!({"c": vec![chain(depth); chains]});
        code_of(push(
            &mut p,
            &mut reporter,
            &json!({"type": "STATE_SNAPSHOT", "snapshot": snapshot}),
        ))
    };
    // chains x (1 + depth) + 1 entries: 148,501 accepted, 150,001 refused.
    assert_eq!(run(1_500, 98), None);
    assert_eq!(run(1_500, 99), Some(ErrorCode::LimitExceeded));
}

/// The entry limit sits below the node ceiling, so a structure-only
/// snapshot the entry limit admits still takes a delta that grows it.
#[test]
fn a_snapshot_at_the_entry_limit_still_takes_growing_deltas() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(PipelineConfig::default());
    push(&mut p, &mut reporter, &started()).unwrap();
    // 149,990 array elements plus the members around them: at the entry
    // limit, under the node ceiling.
    let snapshot = json!({"a": vec![0; 149_990]});
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "STATE_SNAPSHOT", "snapshot": snapshot}),
    )
    .unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "STATE_DELTA", "delta": [
            {"op": "add", "path": "/b", "value": {"x": vec![1; 10_000]}}
        ]}),
    )
    .unwrap();
    assert_eq!(p.reducer().state()["b"]["x"][9_999], 1);
}

/// Updating in place is not growth: the same metadata key on every delta and
/// the same activity path replaced in place stay under a small limit.
#[test]
fn history_limit_is_net_of_overwritten_values() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
    )
    .unwrap();
    let big = "k".repeat(1024);
    for _ in 0..500 {
        let e = json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "a",
            "metadata": {"k": big}});
        push(&mut p, &mut reporter, &e).unwrap();
    }
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
            "content": {"s": big}}),
    )
    .unwrap();
    for _ in 0..500 {
        let e = json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t",
            "patch": [{"op": "replace", "path": "/s", "value": big}]});
        push(&mut p, &mut reporter, &e).unwrap();
    }
}

/// Failing activity deltas that replace a large held value must not cost
/// more than the same frames against a tiny one (the work measuring what a
/// patch frees is banked, and failures spend it).
#[test]
fn failing_activity_deltas_do_not_amplify_by_held_size() {
    let _g = lock();
    let run = |held: usize| {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink, 256);
        let mut p = Processor::new(history_limit(64 * 1024 * 1024));
        push(&mut p, &mut reporter, &started()).unwrap();
        push(
            &mut p,
            &mut reporter,
            &json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
                "content": {"big": "k".repeat(held)}}),
        )
        .unwrap();
        let e = json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t",
            "patch": [{"op": "replace", "path": "/big", "value": 1},
                      {"op": "test", "path": "/nope", "value": 1}]});
        let start = std::time::Instant::now();
        for _ in 0..20_000 {
            let _ = push(&mut p, &mut reporter, &e);
        }
        start.elapsed()
    };
    let small = run(8);
    for held in [100 * 1024, 1024 * 1024] {
        let large = run(held);
        assert!(
            large < small * 8 + std::time::Duration::from_millis(200),
            "held {held}: {large:?} vs small: {small:?}"
        );
    }
}

/// A `move` onto an ancestor of its own source frees nothing; cycling it
/// must not credit the history limit.
#[test]
fn move_to_ancestor_cycle_does_not_free_history_credit() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
            "content": {"a": "k".repeat(16 * 1024)}}),
    )
    .unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
    )
    .unwrap();
    let d = |patch: Value| json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t", "patch": patch});
    for _ in 0..64 {
        for patch in [
            json!([{"op": "add", "path": "/t", "value": {}},
                   {"op": "move", "from": "/a", "path": "/t/b"}]),
            json!([{"op": "move", "from": "/t/b", "path": "/t"}]),
            json!([{"op": "add", "path": "/a", "value": {}},
                   {"op": "move", "from": "/t", "path": "/a/b"}]),
            json!([{"op": "move", "from": "/a/b", "path": "/a"}]),
        ] {
            let _ = push(&mut p, &mut reporter, &d(patch));
        }
    }
    let mut code = None;
    for _ in 0..64 {
        if let Some(c) = code_of(push(&mut p, &mut reporter, &delta("m", 16))) {
            code = Some(c);
            break;
        }
    }
    assert_eq!(code, Some(ErrorCode::LimitExceeded));
}

/// Moves whose source does not exist fail at their first operation, but
/// measuring the patch's credit must stay linear in its size.
#[test]
fn non_applying_moves_credit_measuring_is_linear() {
    let _g = lock();
    let events = |n: usize| -> Vec<Vec<u8>> {
        let moves: Vec<Value> = (0..n)
            .map(|i| json!({"op": "move", "from": format!("/f{i}"), "path": format!("/p{i}")}))
            .collect();
        frames(&[
            started(),
            json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
                "content": {}}),
            json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t",
                "patch": moves}),
        ])
    };
    // 32k operations (4 container entries each) stay under the default
    // 150,000-entry frame budget.
    assert_linear(&events(2_000), &events(32_000));
}

/// Adding and removing a value made of many small objects nets (close to)
/// zero: the credit for the removal never exceeds what the addition was
/// charged, here on empty content, whose map's storage the add charges and
/// the remove credits back.
#[test]
fn add_remove_cycle_of_small_objects_does_not_free_history_credit() {
    let _g = lock();
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut p = Processor::new(history_limit(64 * 1024));
    push(&mut p, &mut reporter, &started()).unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
            "content": {}}),
    )
    .unwrap();
    push(
        &mut p,
        &mut reporter,
        &json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
    )
    .unwrap();
    let value: Vec<Value> = (0..50).map(|_| json!({"a": 0})).collect();
    let d = |patch: Value| json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t", "patch": patch});
    for _ in 0..400 {
        push(
            &mut p,
            &mut reporter,
            &d(json!([{"op": "add", "path": "/x", "value": value}])),
        )
        .unwrap();
        push(
            &mut p,
            &mut reporter,
            &d(json!([{"op": "remove", "path": "/x"}])),
        )
        .unwrap();
    }
    let mut code = None;
    for _ in 0..8 {
        if let Some(c) = code_of(push(&mut p, &mut reporter, &delta("m", 16))) {
            code = Some(c);
            break;
        }
    }
    assert_eq!(code, Some(ErrorCode::LimitExceeded));
}

/// The events of a flood's `i`th round.
type Flood<'a> = Box<dyn Fn(usize) -> Vec<Value> + 'a>;

/// What the pipeline keeps to check and track the stream (open steps, reasoning
/// spans, subagent invocations, the run's item tables, the message ids a
/// snapshot dropped) is charged to the history budget, so none of these floods
/// holds memory past the limit.
#[test]
fn bookkeeping_floods_end_at_the_history_limit() {
    let _g = lock();
    const LIMIT: usize = 8 * 1024 * 1024;
    let long = |i: usize| format!("{i:08}{}", "x".repeat(512 * 1024 - 8));
    let empty = json!({"type": "MESSAGES_SNAPSHOT", "messages": []});
    let rows: Vec<(&str, Flood)> = vec![
        (
            "open steps",
            Box::new(|i| vec![json!({"type": "STEP_STARTED", "stepName": long(i)})]),
        ),
        (
            "reasoning spans",
            Box::new(|i| {
                vec![
                    json!({"type": "REASONING_START", "messageId": long(i)}),
                    json!({"type": "REASONING_END", "messageId": long(i)}),
                ]
            }),
        ),
        (
            "finished subagents",
            Box::new(|i| {
                vec![
                    json!({"type": "SUBAGENT_STARTED", "subagentRunId": long(i), "name": "s"}),
                    json!({"type": "SUBAGENT_FINISHED", "subagentRunId": long(i)}),
                ]
            }),
        ),
        (
            "tool calls dropped by a snapshot",
            Box::new(|i| {
                vec![
                    json!({"type": "TOOL_CALL_START", "toolCallId": long(i), "toolCallName": "f"}),
                    json!({"type": "TOOL_CALL_END", "toolCallId": long(i)}),
                    empty.clone(),
                ]
            }),
        ),
        (
            "text messages dropped by a snapshot",
            Box::new(|i| {
                vec![
                    json!({"type": "TEXT_MESSAGE_START", "messageId": long(i), "role": "assistant"}),
                    json!({"type": "TEXT_MESSAGE_END", "messageId": long(i)}),
                    empty.clone(),
                ]
            }),
        ),
    ];
    for (name, events) in rows {
        let mut reporter = Reporter::new(Arc::new(NullSink), 16);
        let mut p = Processor::new(history_limit(LIMIT));
        push(&mut p, &mut reporter, &started()).unwrap();
        let mut code = None;
        'flood: for i in 0..64 {
            for e in events(i) {
                if let Err(e) = push(&mut p, &mut reporter, &e) {
                    code = Some(e.code());
                    break 'flood;
                }
            }
        }
        assert_eq!(code, Some(ErrorCode::LimitExceeded), "{name}");
    }

    // Short names: a count flood, not a length flood.
    let mut reporter = Reporter::new(Arc::new(NullSink), 16);
    let mut p = Processor::new(history_limit(LIMIT));
    push(&mut p, &mut reporter, &started()).unwrap();
    let mut code = None;
    for i in 0..200_000 {
        let e = json!({"type": "STEP_STARTED", "stepName": format!("step-{i:020}")});
        if let Err(e) = push(&mut p, &mut reporter, &e) {
            code = Some(e.code());
            break;
        }
    }
    assert_eq!(code, Some(ErrorCode::LimitExceeded), "short step names");
}

/// What a flood of retired `THINKING_START` frames leaves resident: the
/// stream's verdict (the error code, if it ended) and the bytes the
/// processor still holds afterwards, measured on this thread.
fn thinking_flood(
    limit: usize,
    rounds: usize,
    round: impl Fn(usize) -> Vec<Value>,
) -> (Option<ErrorCode>, i64) {
    flood_on(Processor::new(history_limit(limit)), rounds, round)
}

/// As [`thinking_flood`], on a given processor.
fn flood_on(
    mut p: Processor,
    rounds: usize,
    round: impl Fn(usize) -> Vec<Value>,
) -> (Option<ErrorCode>, i64) {
    let mut reporter = Reporter::new(Arc::new(NullSink), 16);
    push(&mut p, &mut reporter, &started()).unwrap();
    let batches: Vec<Vec<Vec<u8>>> = (0..rounds).map(|i| frames(&round(i))).collect();
    let mut out = Vec::new();
    let (code, stats) = alloc::measure(|| {
        for batch in &batches {
            for frame in batch {
                if let Err(e) = p.push_json(frame, &mut reporter, &mut out) {
                    return Some(e.code());
                }
                out.clear();
            }
        }
        None
    });
    drop(p);
    (code, stats.live())
}

/// The retired-thinking shim keeps the spans it translated so a later
/// unnamed `THINKING_END` can be named. A span closed by a 1.0
/// `REASONING_END` (which the shim does not translate) is closed in the
/// shim too, the same id opened again is one entry, and what the shim
/// holds is charged to the history budget with the rest of the id
/// bookkeeping (D25, D28). Before, every `THINKING_START X` /
/// `REASONING_END X` pair left one uncharged copy of `X` behind.
#[test]
fn thinking_spans_closed_by_reasoning_end_hold_no_memory() {
    let _g = lock();
    const LIMIT: usize = 8 * 1024 * 1024;
    let long = |i: usize| format!("{i:08}{}", "x".repeat(512 * 1024 - 8));

    // The same 512 KiB id, opened and closed 64 times: the verifier charges
    // it once, and nothing else may keep a copy per pair.
    let same = long(0);
    let (code, live) = thinking_flood(LIMIT, 64, |_| {
        vec![
            json!({"type": "THINKING_START", "messageId": same}),
            json!({"type": "REASONING_END", "messageId": same}),
        ]
    });
    assert_eq!(code, None, "same id: the stream is conformant");
    assert!(
        live < 4 * 1024 * 1024,
        "same id: {} KiB resident after 64 pairs of a 512 KiB id",
        live / 1024
    );

    // Distinct ids: the stream ends at the limit, and the shim adds no
    // second copy of each id on top of what the verifier is charged.
    // Open spans (no end at all) are charged through the shim's own figure.
    let distinct: Vec<(&str, Flood)> = vec![
        (
            "closed by REASONING_END",
            Box::new(|i| {
                vec![
                    json!({"type": "THINKING_START", "messageId": long(i)}),
                    json!({"type": "REASONING_END", "messageId": long(i)}),
                ]
            }),
        ),
        (
            "left open",
            Box::new(|i| vec![json!({"type": "THINKING_START", "messageId": long(i)})]),
        ),
    ];
    for (name, round) in distinct {
        let (code, live) = thinking_flood(LIMIT, 64, round);
        assert_eq!(code, Some(ErrorCode::LimitExceeded), "distinct ids {name}");
        assert!(
            live < i64::try_from(LIMIT + LIMIT / 2).unwrap(),
            "distinct ids {name}: {} KiB resident at a {} KiB limit",
            live / 1024,
            LIMIT / 1024
        );
    }
}

/// A subagent's `TOOL_CALL_CHUNK` stream checks later chunks' repeated
/// `parentMessageId` against the opener's, and one such stream can stay
/// open per active subagent. The comparison keeps a fingerprint, not a copy
/// of the peer's id: before, every subagent lane kept its own uncharged copy
/// of the opener's `parentMessageId`, so naming one long message as the
/// parent from many subagents held memory in proportion to the wire with no
/// error (D25).
#[test]
fn subagent_tool_chunk_lanes_hold_no_copy_of_the_parent_id() {
    let _g = lock();
    const LIMIT: usize = 8 * 1024 * 1024;
    let parent = format!("p{}", "x".repeat(512 * 1024 - 1));
    let rounds = || {
        let parent = parent.clone();
        move |i: usize| {
            vec![
                json!({
                    "type": "SUBAGENT_STARTED", "subagentRunId": format!("s{i}"), "name": "h"
                }),
                json!({
                    "type": "TOOL_CALL_CHUNK",
                    "toolCallId": format!("c{i}"),
                    "toolCallName": "f",
                    "parentMessageId": parent,
                    "subagentRunId": format!("s{i}")
                }),
            ]
        }
    };
    // 64 subagents naming one 512 KiB parent: 32 MiB on the wire, one copy
    // of the parent held (by the history), whether the restored history or
    // the first chunk's minted carrier put it there. The consumer accepts
    // both streams: it knows no owner for a message held from before the
    // run, nor for a carrier the reducer minted, so it takes each lane's tag
    // at its word. (A parent this run streamed or a snapshot of it restated
    // has a known owner, and a lane tagged with another is refused.)
    let held: Vec<agui_sdk::message::Message> =
        serde_json::from_value(json!([{"id": parent, "role": "assistant"}])).unwrap();
    let cases = [
        (
            "held parent",
            Processor::with_history(history_limit(LIMIT), held, json!({})),
        ),
        ("minted carrier", Processor::new(history_limit(LIMIT))),
    ];
    for (name, p) in cases {
        let (code, live) = flood_on(p, 64, rounds());
        assert_eq!(code, None, "{name}: the consumer accepts the stream");
        assert!(
            live < 4 * 1024 * 1024,
            "{name}: {} KiB resident after 64 subagent lanes naming a 512 KiB parent",
            live / 1024
        );
    }
}

/// An application that raises `max_depth` past what the JSON walk can
/// recurse through on a thread's stack still gets a verdict, not a stack
/// overflow: the JSON depth is clamped to a fixed ceiling, as the patch
/// engine's and the protobuf codec's are. The ceiling itself is reachable on
/// a default-sized (2 MiB) thread stack in an optimized build; an
/// unoptimized one has far larger frames, so the test gives it more stack.
#[test]
fn a_raised_max_depth_is_clamped_to_a_stack_safe_ceiling() {
    let _g = lock();
    let deep = |n: usize| {
        format!(
            r#"{{"type":"CUSTOM","name":"n","value":{}1{}}}"#,
            "[".repeat(n),
            "]".repeat(n)
        )
    };
    std::thread::Builder::new()
        .stack_size(if cfg!(debug_assertions) {
            16 << 20
        } else {
            2 << 20
        })
        .spawn(move || {
            let mut config = PipelineConfig::default();
            config.limits.max_depth = usize::MAX;
            let push = |frame: &str| {
                let mut p = Processor::new(config.clone());
                let mut reporter = Reporter::new(Arc::new(NullSink), 256);
                let mut out = Vec::new();
                p.push_json(started().to_string().as_bytes(), &mut reporter, &mut out)
                    .unwrap();
                p.push_json(frame.as_bytes(), &mut reporter, &mut out)
                    .map(|_| ())
                    .map_err(|e| e.code())
            };
            assert_eq!(push(&deep(100_000)), Err(ErrorCode::LimitExceeded));
            assert_eq!(push(&deep(500)), Ok(()));
        })
        .unwrap()
        .join()
        .unwrap();
}
