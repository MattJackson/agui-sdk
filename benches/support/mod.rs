//! The bench harness (TEST-PLAN §9, docs/PERFORMANCE.md). No criterion.
//!
//! Every bench binary installs the counting allocator, declares its cases and
//! hands control to [`Bench::finish`], which exits non-zero on any breach:
//!
//! * **allocations per event** (hard, exact): the *marginal* count between
//!   the smallest and largest size, so fixed setup costs cancel and what is
//!   left is the steady-state cost per event. It must equal
//!   `allocs_per_event` in `benches/budgets.toml`; a case with no budget
//!   fails until one is recorded. Cases may also assert a computed ceiling
//!   (the formula in TEST-PLAN §9.1). The budgets are the default build's,
//!   which is what CI's perf job runs. `preserve-order` (and so
//!   `--all-features`) makes `serde_json` objects `IndexMap`s instead of
//!   `BTreeMap`s, which changes the count wherever the measured path builds
//!   objects; such a case sets `map_dependent = true`, and a
//!   `preserve-order` build prints its count without gating it. Every other
//!   budget, linearity and the computed ceilings are gated in every build.
//! * **linearity** (hard): from ×1 to ×8 input, allocations may grow at most
//!   8 × `alloc_linearity` and time at most 8 × `time_linearity`. A
//!   super-linear path (×8 input, ×64 cost) fails by a wide margin. A case
//!   whose ×8 input outgrows the CPU caches may set its own, looser
//!   `time_linearity` for that range and add `time_scales = [a, b]`: two
//!   more sizes (fractions of the base allowed) whose time ratio is held to
//!   `[gate] time_scales_linearity`, its own bound: 8 × 2 = ×16 for ×8
//!   input, where `[gate] time_linearity`'s ×12 flaked at ×12.15 on CI, and
//!   a quadratic is still ×64 (docs/PERFORMANCE.md, "Time linearity and the
//!   cache").
//! * **throughput floor** (hard, loose, optional): `min_mb_per_s` catches
//!   cliffs, not drift. Unset floors only print.
//!
//! Modes:
//!
//! * `cargo bench` (the binary gets `--bench`): sizes ×1 and ×8, gated.
//!   `AGUI_BENCH_MODE=full` adds ×2 and ×4 and more iterations.
//! * `cargo test --benches` / `--all-targets` (no `--bench`): smoke run, one
//!   pass on a small input, nothing gated.
//! * `--profile-time N`: loop the largest size for N seconds per case, no
//!   gate (for `cargo flamegraph`, `samply`, `perf`).
//!
//! `AGUI_BENCH_OUT=<dir>` writes `<dir>/<bench>.json` with every measurement.

#![allow(dead_code, unused_imports)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use agui_test_support::alloc::{self, AllocStats};
use serde_json::{json, Value};

pub use agui_test_support::alloc::CountingAllocator;

/// What one pass of a workload did.
#[derive(Debug, Clone, Copy, Default)]
pub struct Work {
    /// Events processed.
    pub events: u64,
    /// Input bytes processed.
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Smoke,
    Quick,
    Full,
    Profile(u64),
}

#[derive(Debug, Clone)]
struct Point {
    scale: f64,
    work: Work,
    allocs: AllocStats,
    median_ns: f64,
    p95_ns: f64,
}

struct Budget {
    allocs_per_event: Option<f64>,
    /// The count depends on `serde_json`'s map type, so it is the default
    /// build's and is not gated under `preserve-order`.
    map_dependent: bool,
    min_mb_per_s: f64,
    /// Overrides `[gate] time_linearity` over the mode's ×1 to ×8 range.
    time_linearity: Option<f64>,
    /// Two more sizes, as multiples of the base, whose time ratio is held to
    /// `[gate] time_scales_linearity`.
    time_scales: Option<(f64, f64)>,
}

/// A computed ceiling on one measured pass.
pub type Ceiling = dyn Fn(&Work, &AllocStats) -> Result<(), String>;

/// One bench binary's run.
pub struct Bench {
    name: &'static str,
    mode: Mode,
    alloc_linearity: f64,
    time_linearity: f64,
    /// `[gate] time_scales_linearity`: the bound of a case's `time_scales`
    /// pair.
    time_scales_linearity: f64,
    budgets: BTreeMap<String, Budget>,
    failures: Vec<String>,
    results: Vec<Value>,
}

/// The `[gate] time_linearity` limit from `budgets.toml`.
pub fn gate_time_linearity() -> f64 {
    load_budgets().time_linearity
}

/// What `budgets.toml` holds: `[gate]` and the per-case budgets.
struct Budgets {
    alloc_linearity: f64,
    time_linearity: f64,
    time_scales_linearity: f64,
    cases: BTreeMap<String, Budget>,
}

fn load_budgets() -> Budgets {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/benches/budgets.toml");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let doc: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
    let num = |v: Option<&toml::Value>| {
        v.and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
    };
    let gate = doc.get("gate").and_then(toml::Value::as_table);
    let alloc_lin = num(gate.and_then(|g| g.get("alloc_linearity"))).unwrap_or(1.15);
    let time_lin = num(gate.and_then(|g| g.get("time_linearity"))).unwrap_or(1.5);
    let time_scales_lin = num(gate.and_then(|g| g.get("time_scales_linearity"))).unwrap_or(2.0);
    let mut budgets = BTreeMap::new();
    if let Some(cases) = doc.get("case").and_then(toml::Value::as_table) {
        for (name, case) in cases {
            budgets.insert(
                name.clone(),
                Budget {
                    allocs_per_event: num(case.get("allocs_per_event")),
                    map_dependent: case
                        .get("map_dependent")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false),
                    min_mb_per_s: num(case.get("min_mb_per_s")).unwrap_or(0.0),
                    time_linearity: num(case.get("time_linearity")),
                    time_scales: case
                        .get("time_scales")
                        .and_then(toml::Value::as_array)
                        .and_then(|v| match v.as_slice() {
                            [a, b] => num(Some(a)).zip(num(Some(b))),
                            _ => None,
                        })
                        .filter(|&(a, b)| 0.0 < a && a < b)
                        .or_else(|| {
                            case.get("time_scales").map(|_| {
                                panic!("{path}: {name}: time_scales must be [a, b] with 0 < a < b")
                            })
                        }),
                },
            );
        }
    }
    Budgets {
        alloc_linearity: alloc_lin,
        time_linearity: time_lin,
        time_scales_linearity: time_scales_lin,
        cases: budgets,
    }
}

impl Bench {
    /// Reads the mode from the command line and environment.
    pub fn new(name: &'static str) -> Self {
        let args: Vec<String> = std::env::args().collect();
        let profile = args.iter().position(|a| a == "--profile-time").map(|i| {
            let value = args.get(i + 1).map(String::as_str);
            value
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or_else(|| panic!("usage: --profile-time <SECONDS> (got {value:?})"))
        });
        let mode = match profile {
            Some(secs) => Mode::Profile(secs),
            None if !args.iter().any(|a| a == "--bench") => Mode::Smoke,
            None if std::env::var("AGUI_BENCH_MODE").as_deref() == Ok("full") => Mode::Full,
            None => Mode::Quick,
        };
        let budgets = load_budgets();
        Bench {
            name,
            mode,
            alloc_linearity: budgets.alloc_linearity,
            time_linearity: budgets.time_linearity,
            time_scales_linearity: budgets.time_scales_linearity,
            budgets: budgets.cases,
            failures: Vec::new(),
            results: Vec::new(),
        }
    }

    fn scales(&self) -> &'static [usize] {
        match self.mode {
            Mode::Smoke => &[1],
            Mode::Quick => &[1, 8],
            Mode::Full => &[1, 2, 4, 8],
            Mode::Profile(_) => &[8],
        }
    }

    fn iterations(&self) -> usize {
        match self.mode {
            Mode::Smoke | Mode::Profile(_) => 1,
            Mode::Quick => 7,
            Mode::Full => 21,
        }
    }

    /// Runs one case. `base` is the workload size at ×1, in the factory's own
    /// units; `factory(units)` returns `(prepare, run)`: `prepare` builds a
    /// fresh input (not measured), `run` consumes it (measured).
    pub fn case<I, P, R>(
        &mut self,
        case: &str,
        base: usize,
        factory: impl Fn(usize) -> (P, R),
        ceiling: Option<&Ceiling>,
    ) where
        P: FnMut() -> I,
        R: FnMut(I) -> Work,
    {
        let full = format!("{}/{case}", self.name);
        if self.mode == Mode::Smoke {
            let (mut prepare, mut run) = factory((base / 64).max(1));
            let input = prepare();
            let work = run(input);
            println!("{full}: smoke ok ({} events)", work.events);
            return;
        }
        if let Mode::Profile(secs) = self.mode {
            let (mut prepare, mut run) = factory(base * 8);
            let until = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < until {
                let input = prepare();
                std::hint::black_box(run(input));
            }
            return;
        }
        if !alloc::installed() {
            self.failures
                .push(format!("{full}: the counting allocator is not installed"));
            return;
        }
        let time_scales = self.budgets.get(&full).and_then(|b| b.time_scales);
        let mut scales: Vec<f64> = self.scales().iter().map(|&s| s as f64).collect();
        scales.extend(time_scales.into_iter().flat_map(|(a, b)| [a, b]));
        scales.sort_by(f64::total_cmp);
        scales.dedup();
        let mut points = Vec::new();
        for scale in scales {
            let units = ((base as f64 * scale).round() as usize).max(1);
            let (mut prepare, mut run) = factory(units);
            // Warm-up, then one counted pass, then the timed passes.
            let input = prepare();
            run(input);
            let input = prepare();
            let (work, allocs) = alloc::measure(|| run(input));
            let mut times = Vec::new();
            for _ in 0..self.iterations() {
                let input = prepare();
                let start = Instant::now();
                std::hint::black_box(run(input));
                times.push(start.elapsed().as_nanos() as f64);
            }
            times.sort_by(f64::total_cmp);
            let per = |ns: f64| ns / work.events.max(1) as f64;
            let p95 = times[((times.len() as f64 * 0.95).ceil() as usize).min(times.len()) - 1];
            points.push(Point {
                scale,
                work,
                allocs,
                median_ns: per(times[times.len() / 2]),
                p95_ns: per(p95),
            });
            if let Some(check) = ceiling {
                if let Err(e) = check(&work, &allocs) {
                    self.failures.push(format!("{full} ×{scale}: ceiling: {e}"));
                }
            }
        }
        self.judge(&full, &points);
    }

    fn judge(&mut self, full: &str, points: &[Point]) {
        let at = |scale: f64| points.iter().find(|p| p.scale == scale);
        let mode = self.scales();
        let (Some(first), Some(last)) = (
            mode.first().and_then(|&s| at(s as f64)),
            mode.last().and_then(|&s| at(s as f64)),
        ) else {
            return;
        };
        let k = last.scale / first.scale;
        let d_events = last.work.events.saturating_sub(first.work.events);
        let marginal = if d_events == 0 {
            last.allocs.total() as f64 / last.work.events.max(1) as f64
        } else {
            (last.allocs.total() as f64 - first.allocs.total() as f64) / d_events as f64
        };
        let mb_s =
            last.work.bytes as f64 / (last.median_ns * last.work.events.max(1) as f64 / 1e9) / 1e6;
        println!(
            "{full}: {:.1} ns/event median, {:.1} p95, {:.1} MB/s, {:.3} allocs/event (marginal), {} allocs total at ×{}",
            last.median_ns, last.p95_ns, mb_s, marginal, last.allocs.total(), last.scale
        );

        let budget = self.budgets.get(full);
        let ungated = cfg!(feature = "preserve-order") && budget.is_some_and(|b| b.map_dependent);
        match budget.and_then(|b| b.allocs_per_event) {
            Some(want) if ungated => println!(
                "{full}: allocs/event not gated: `preserve-order` is on, and the budget ({want}) \
                 is the default build's (BTreeMap objects allocate differently); \
                 run `cargo bench` with default features to check it"
            ),
            None => self.failures.push(format!(
                "{full}: no allocs_per_event in benches/budgets.toml (measured {marginal:.3}); record it"
            )),
            Some(want) if (want - marginal).abs() > 0.005 => self.failures.push(format!(
                "{full}: {marginal:.3} allocs/event, budget is exactly {want}: a change in either direction needs a budget edit in the same PR"
            )),
            Some(_) => {}
        }
        if first.allocs.total() > 0 {
            let ratio = last.allocs.total() as f64 / first.allocs.total() as f64;
            if ratio > k * self.alloc_linearity {
                self.failures.push(format!(
                    "{full}: allocations grew ×{ratio:.2} for ×{k} input (limit ×{:.2}): super-linear",
                    k * self.alloc_linearity
                ));
            }
        }
        let total = |p: &Point| p.median_ns * p.work.events.max(1) as f64;
        let mut time_checks = vec![(
            first,
            last,
            budget
                .and_then(|b| b.time_linearity)
                .unwrap_or(self.time_linearity),
        )];
        if let Some((a, b)) = budget.and_then(|b| b.time_scales) {
            match (at(a), at(b)) {
                (Some(a), Some(b)) => time_checks.push((a, b, self.time_scales_linearity)),
                _ => self
                    .failures
                    .push(format!("{full}: time_scales [{a}, {b}] were not measured")),
            }
        }
        for &(small, large, linearity) in &time_checks {
            let k = large.scale / small.scale;
            let ratio = total(large) / total(small).max(1.0);
            println!(
                "{full}: time ×{ratio:.2} from ×{} to ×{} (limit ×{:.2})",
                small.scale,
                large.scale,
                k * linearity
            );
            if ratio > k * linearity {
                self.failures.push(format!(
                    "{full}: time grew ×{ratio:.2} for ×{k} input, ×{} to ×{} (limit ×{:.2}): super-linear",
                    small.scale,
                    large.scale,
                    k * linearity
                ));
            }
        }
        let floor = budget.map_or(0.0, |b| b.min_mb_per_s);
        if floor > 0.0 && last.work.bytes > 0 && mb_s < floor {
            self.failures.push(format!(
                "{full}: {mb_s:.1} MB/s is below the floor of {floor} MB/s"
            ));
        }
        self.results.push(json!({
            "case": full,
            "marginal_allocs_per_event": marginal,
            "mb_per_s": mb_s,
            "points": points.iter().map(|p| json!({
                "scale": p.scale, "events": p.work.events, "bytes": p.work.bytes,
                "allocations": p.allocs.allocations, "reallocations": p.allocs.reallocations,
                "alloc_bytes": p.allocs.bytes, "median_ns_per_event": p.median_ns, "p95_ns_per_event": p.p95_ns,
            })).collect::<Vec<_>>(),
        }));
    }

    /// Reports and exits: non-zero when anything breached.
    pub fn finish(self) -> ! {
        if let Ok(dir) = std::env::var("AGUI_BENCH_OUT") {
            let path = std::path::Path::new(&dir).join(format!("{}.json", self.name));
            let shown = path.display();
            // Diagnostic artifact: warn and degrade, the verdict never depends on it.
            match std::fs::create_dir_all(&dir) {
                Err(e) => eprintln!("PERF OUT: could not create {dir}: {e}"),
                Ok(()) => match serde_json::to_vec_pretty(&self.results) {
                    Err(e) => eprintln!("PERF OUT: could not serialize results for {shown}: {e}"),
                    Ok(bytes) => {
                        if let Err(e) = std::fs::write(&path, bytes) {
                            eprintln!("PERF OUT: could not write {shown}: {e}");
                        }
                    }
                },
            }
        }
        if self.failures.is_empty() {
            std::process::exit(0);
        }
        for f in &self.failures {
            eprintln!("PERF GATE: {f}");
        }
        std::process::exit(1);
    }
}

/// A text-heavy stream of `runs` complete runs: per run a text message with
/// 40 deltas, a tool call with 10 argument deltas and its result, a state
/// snapshot and a delta. Typical traffic: content deltas dominate.
pub fn typical_stream(runs: usize) -> Vec<Value> {
    let mut out = Vec::with_capacity(runs * 64);
    for r in 0..runs {
        let (t, run, m, tc) = (
            "t-bench".to_owned(),
            format!("r-{r}"),
            format!("m-{r}"),
            format!("tc-{r}"),
        );
        out.push(json!({"type": "RUN_STARTED", "threadId": t, "runId": run}));
        out.push(json!({"type": "TEXT_MESSAGE_START", "messageId": m, "role": "assistant"}));
        for i in 0..40 {
            out.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": m, "delta": format!("token {i} of a streamed answer, ")}));
        }
        out.push(json!({"type": "TEXT_MESSAGE_END", "messageId": m}));
        out.push(json!({"type": "TOOL_CALL_START", "toolCallId": tc, "toolCallName": "search", "parentMessageId": m}));
        for i in 0..10 {
            out.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": tc, "delta": format!("{{\"q{i}\":\"v\"}}")}));
        }
        out.push(json!({"type": "TOOL_CALL_END", "toolCallId": tc}));
        out.push(json!({"type": "TOOL_CALL_RESULT", "messageId": format!("{m}-tool"), "toolCallId": tc, "content": "result text"}));
        out.push(json!({"type": "STATE_SNAPSHOT", "snapshot": {"step": r, "items": ["a", "b"]}}));
        out.push(json!({"type": "STATE_DELTA", "delta": [{"op": "replace", "path": "/step", "value": r + 1}]}));
        out.push(json!({"type": "RUN_FINISHED", "threadId": t, "runId": run}));
    }
    out
}

/// Each event serialized once.
pub fn frames(events: &[Value]) -> Vec<Vec<u8>> {
    events
        .iter()
        .map(|e| serde_json::to_vec(e).unwrap_or_default())
        .collect()
}

/// Strings an event owns once parsed, plus one per array buffer (keys are
/// fixed field names and are not owned). The ceiling for pipeline allocations.
pub fn owned_strings(value: &Value) -> u64 {
    match value {
        Value::String(_) => 1,
        Value::Array(items) => 1 + items.iter().map(owned_strings).sum::<u64>(),
        Value::Object(map) => map
            .iter()
            .filter(|(k, _)| *k != "type")
            .map(|(_, v)| owned_strings(v))
            .sum(),
        _ => 0,
    }
}
