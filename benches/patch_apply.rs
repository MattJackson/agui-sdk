//! `patch::apply`: 1k-operation patches on a ~1 MB document, succeeding and
//! failing on the last operation. Ceilings: no document clone (allocated
//! bytes stay within 8x the operations' own size), and a
//! failed patch leaves the document bit-identical (TEST-PLAN §9.1).

mod support;

use agui_sdk::patch::{apply, PatchBudget, PatchLimits, PatchOperation};
use agui_test_support::alloc::AllocStats;
use serde_json::{json, Value};
use std::{cell::RefCell, rc::Rc};
use support::{Bench, CountingAllocator, Work};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn document() -> Value {
    // ~1 MB: 10k entries of ~100 bytes.
    let items: serde_json::Map<String, Value> = (0..10_000)
        .map(|i| (format!("k{i}"), json!({"name": format!("item {i}"), "tags": ["a", "b", "c"], "n": i, "pad": "x".repeat(40)})))
        .collect();
    json!({ "items": items, "log": [] })
}

fn ops(n: usize, fail: bool) -> (Vec<PatchOperation>, u64) {
    let mut v: Vec<Value> = (0..n)
        .map(|i| match i % 3 {
            0 => json!({"op": "replace", "path": format!("/items/k{}/n", i % 10_000), "value": i}),
            1 => json!({"op": "add", "path": "/log/-", "value": {"i": i}}),
            _ => json!({"op": "remove", "path": format!("/items/k{}/pad", i % 10_000)}),
        })
        .collect();
    if fail {
        if let Some(last) = v.last_mut() {
            *last = json!({"op": "remove", "path": "/does/not/exist"});
        }
    }
    let size = serde_json::to_vec(&v).map_or(0, |b| b.len() as u64);
    (
        serde_json::from_value(Value::Array(v)).unwrap_or_else(|e| panic!("{e}")),
        size,
    )
}

/// Holds the document `run` finished with, so its comparison and drop stay
/// out of the timed region. `check` runs in the next `prepare`; `Drop` checks
/// whatever is still parked when the case ends (the only pass in smoke mode).
struct Parked {
    original: Value,
    fail: bool,
    doc: RefCell<Option<Value>>,
}

impl Parked {
    fn check(&self) {
        let done = self.doc.borrow_mut().take();
        if let (true, Some(done)) = (self.fail, done) {
            assert!(
                done == self.original,
                "a failed patch must leave the document unchanged"
            );
        }
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.check();
        }
    }
}

fn main() {
    let mut bench = Bench::new("patch_apply");
    let base = document();
    for (case, fail) in [("apply", false), ("failing", true)] {
        let base = base.clone();
        // Per operation: its operand cloned in (a cloned object costs one
        // `serde_json` map leaf, ~630 bytes, for `{"i": n}`) and one undo
        // entry (104 bytes), reserved once. A document clone also breaks the
        // exact allocation budget (tens of thousands of nodes).
        let ceiling = |w: &Work, a: &AllocStats| {
            if a.bytes <= 8 * w.bytes + 4096 {
                Ok(())
            } else {
                Err(format!(
                    "{} bytes allocated for {} bytes of operations: a document clone?",
                    a.bytes, w.bytes
                ))
            }
        };
        bench.case(
            case,
            1000,
            move |n| {
                let (ops, size) = ops(n, fail);
                let base = base.clone();
                let original = base.clone();
                // The timed `run` must not do O(document) work (dropping the
                // document, comparing it with the original): that fixed cost
                // would hide a super-linear `apply` from the time gate. `run`
                // parks the finished document here; the untimed `prepare` of
                // the next pass checks and drops it.
                let parked = Rc::new(Parked {
                    original,
                    fail,
                    doc: RefCell::default(),
                });
                let park = Rc::clone(&parked);
                let prepare = move || {
                    park.check();
                    let budget = PatchBudget::for_document(&base, PatchLimits::default());
                    (base.clone(), budget)
                };
                let run = move |(mut doc, mut budget): (Value, PatchBudget)| {
                    let result = apply(&mut doc, &ops, &mut budget);
                    assert_eq!(result.is_err(), fail, "{result:?}");
                    *parked.doc.borrow_mut() = Some(doc);
                    Work {
                        events: ops.len() as u64,
                        bytes: size,
                    }
                };
                (prepare, run)
            },
            Some(&ceiling),
        );
    }
    // Only `preserve-order` can rescan (BTreeMap removal is O(log n)).
    // Rolling back k added members of an object of 20k members must not
    // rescan it per member (O(k) total, not O(k x members)): input and
    // object grow together, so a rescan shows as ×64 time for ×8 input.
    #[cfg(feature = "preserve-order")]
    bench.case(
        "wide_object_rollback",
        500,
        |k| {
            let wide: serde_json::Map<String, Value> =
                (0..20 * k).map(|i| (format!("k{i}"), json!(0))).collect();
            let wide = Value::Object(wide);
            let mut v: Vec<Value> = (0..k)
                .map(|i| json!({"op": "add", "path": format!("/new{i}"), "value": 1}))
                .collect();
            v.push(json!({"op": "test", "path": "/k0", "value": 1}));
            let ops: Vec<PatchOperation> =
                serde_json::from_value(Value::Array(v)).unwrap_or_else(|e| panic!("{e}"));
            let prepare = move || {
                let budget = PatchBudget::for_document(&wide, PatchLimits::default());
                (wide.clone(), budget)
            };
            let run = move |(mut doc, mut budget): (Value, PatchBudget)| {
                assert!(apply(&mut doc, &ops, &mut budget).is_err());
                std::hint::black_box(doc);
                Work {
                    events: ops.len() as u64,
                    bytes: 0,
                }
            };
            (prepare, run)
        },
        None,
    );
    bench.finish();
}
