# Performance

Performance is a gate, not an afterthought. `agui-sdk` sits on the hot path of
every token an agent streams to a user, and every byte it parses comes from a
peer it does not control. So two different things are held to account:

- **Cost per event** (throughput and allocations), because it is paid on every
  token of every run.
- **How cost grows with input size** (linearity), because a parser that is
  accidentally quadratic is a denial-of-service vector that functional tests on
  small inputs never see.

## The gate

`.github/workflows/perf.yml`, job `gate`, runs on every qa run (qa.yml calls
it) and therefore on every release. It fails when any of these is breached:

| Check | Measured as | Threshold style |
| --- | --- | --- |
| **Allocation budget** | exact allocation count (and bytes) per operation, from a counting allocator | tight: allocation counts are deterministic, so any increase is a real change |
| **Linearity** | cost at input size N and k·N, for time and allocations | ratio must stay near k (bounded by a stated factor); a super-linear path fails |
| **Throughput floor** | events/s or bytes/s on the CI runner | loose: shared runners are noisy, so this catches cliffs, not percent-level drift |

The gate step is `cargo bench --locked -p agui-sdk --benches` (perf.yml, job
`gate`). The bench targets are in `benches/`: `sse_decode`, `proto_decode`,
`pipeline_push_json`, `pipeline_push_value`, `reducer_deltas`,
`messages_snapshot`, `patch_apply`, `server_emit_encode`, `client_run`,
`pipeline_linearity`, `protocol_types` and `http_transport`. The numbers below
are the ones recorded in `benches/budgets.toml`, which is the source of truth.

Linearity applies to every gated case alike: going from x1 to x8 input may
cost at most 8 x 1.15 in allocations and 8 x 2.0 in time (`[gate]` in
`benches/budgets.toml`). The exceptions, for time only, are the cases whose x8
outgrows the CPU caches, `messages_snapshot/reconcile` first (see "Time
linearity and the cache" below). No throughput floor is set yet: every case has
`min_mb_per_s = 0`, to be set from a green run on the reference runner.
Allocation counts are the default-feature build's (the CI gate's).
`preserve-order`, and so `--all-features`, makes `serde_json` objects
`IndexMap`s, which changes the count of every case that builds objects
(`pipeline_push_json/text_heavy` measures 1.576 there, for example). Those
cases carry `map_dependent = true` in `benches/budgets.toml`, and a
`preserve-order` build prints their counts without gating them; the other
budgets, linearity and the computed ceilings stay gated. The bench
streams are written by `serde_json` with sorted keys, so `type` comes last in
every frame, the harder order for a tagged parse.

| Hot path | Allocation budget (per event) | Linearity bound | Throughput floor |
| --- | --- | --- | --- |
| SSE decode (`sse_decode/1KiB`, `/64KiB`) | 0 | 8 x 1.15 allocs, 8 x 2.0 time | not set |
| JSON event through the pipeline (`pipeline_push_json/text_heavy`) | 1.542 (below the owned-strings ceiling asserted in the bench) | 8 x 1.15 allocs, 8 x 2.0 time | not set |
| Middleware path, `Value` in (`pipeline_push_value/text_heavy`) | 0.576 | 8 x 1.15 allocs, 8 x 2.0 time | not set |
| Protobuf framing (`proto_decode/framing`) | 0 | 8 x 1.15 allocs, 8 x 2.0 time | not set |
| Protobuf decode to a `Value` (`proto_decode/decode`) | 7.119 (the value's own nodes) | 8 x 1.15 allocs, 8 x 2.0 time | not set |
| Event encode with producer verification (`server_emit_encode/sse`, `/proto`) | 0 each | 8 x 1.15 allocs, 8 x 2.0 time | not set |
| `STATE_DELTA` JSON Patch apply (`patch_apply/apply`, `/failing`), per operation | 0.667 each (cloned operands; bytes ceiling asserted in the bench) | 8 x 1.15 allocs, 8 x 2.0 time | not set |
| Message accumulation across a run (`reducer_deltas/one_message`, `/one_message_held`) | 0 when updates are dropped; 1 (one `Arc<Event>`) when held | 8 x 1.15 allocs, 8 x 2.0 time | not set |
| `MESSAGES_SNAPSHOT` reconcile, per message (`messages_snapshot/reconcile`) | 1 | 8 x 1.15 allocs; time 8 x 2 from 1.25k to 10k messages and 8 x 4 from 10k to 80k (see below) | not set |
| Tiny `MESSAGES_SNAPSHOT`s against held activity (`messages_snapshot/tiny_snapshots_vs_held_activity`) | 1 | 3.0 time | not set |
| Reopening a message under a new parent (`reducer_deltas/reopen_new_parent`) | 1 | 3.0 time | not set |
| Client run loop (`client_run/await`, `/iterate_observed`) | 1.542 each (equal to the pipeline: the client adds 0) | 8 x 1.15 allocs, 8 x 2.0 time | not set |

### Time linearity and the cache

Time ratios assume the cost per element is the same at both sizes. That
holds while the working set fits the CPU caches, and stops holding once it
does not: every access then pays for memory, so the time per element steps up
even though the work per element is unchanged. `messages_snapshot/reconcile`
is the case where this bites. Its x1 is 10k messages and its x8 is 80k, and
the 80k history, its index and the snapshot's messages outgrow the caches.
Measured on an Apple M-series laptop (median ns per message, full mode):

| Messages | 1.25k | 2.5k | 5k | 10k | 20k | 40k | 80k | 160k | 320k | 640k |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ns per message | 93 | 95 | 96 | 98-105 | 108-115 | 123-141 | 138-163 | 196 | 264 | 205 |

The cost is flat inside the cache and wobbles above it, falling again at
640k, which a quadratic never does (with `a + b·n` fitted to 10k-80k it would
be about 540 ns at 640k). Over 10k to 80k the time therefore grew x12.6-14
for x8 input, and the strict bound of 8 x 1.5 = x12 flaked.

The fix does not loosen the check for quadratic code. The case gets two
checks (`time_linearity` and `time_scales` on the case in
`benches/budgets.toml`, applied by `benches/support/mod.rs`):

- **In cache, tight.** `time_scales = [0.125, 1]` adds a run at 1.25k
  messages. From 1.25k to 10k (x8 input, both sizes in cache) the time ratio
  is held to `[gate] time_scales_linearity`, 8 x 2 = x16. Measured x7.9-9.5
  locally, also with the machine under load; the qa runner once measured
  x12.15, so the in-cache pair has its own tolerance rather than the
  `time_linearity` bound of 8 x 1.5 = x12, which flaked.
- **Out of cache, loose.** From 10k to 80k (the usual x1 to x8) the bound is
  8 x 4 = x32, which covers the cache step (measured up to x18.6 under load
  locally, and x18.3 to x26.0 on shared CI runners on unchanged code).

A real O(n²) costs x64 for x8 input, so it fails both checks, the in-cache
one by 4x. A quadratic term too small to show at 10k still shows at
80k: with an extra `n²/200` loop injected into the bench, the in-cache ratio
was x10.6 (passing) and the out-of-cache one x29. With the CI-safe x32
bound that term passes the bench too, so the bench is the guard for gross
quadratics only. Smaller quadratic terms in reconcile are the job of the
wall-clock tests in `tests/hostile_peers.rs` (`messages_snapshot_*_is_linear`):
16x the input against a 64x bound, where linear is about 16x and quadratic
about 256x. The wide check stays as the bench's own guard. Allocation
linearity and the exact allocation budget are unaffected: they still use
10k and 80k, because allocation counts do not depend on the cache.

A case that hits the same effect later gets the same treatment: an in-cache
`time_scales` pair at the in-cache bound (x16), and a looser `time_linearity`
over the usual range, chosen so that x64 still fails. Never a looser bound alone.
`reducer_deltas/one_message_held` is the second such case: every kept update
stays live, 22 MB at x1 and 177 MB at x8, and the qa runner measured x17.08
for x1 to x8 while 12.5k to 100k deltas stay at x8.1-9.6 (figures and bounds
in `benches/budgets.toml`). `server_emit_encode/sse` and `/proto` are the
third: the pre-built events are the working set and the bytes per event stay
flat; on an M-series laptop the time per event stays within 1.2x from x0.125
to x64, while the qa runner measured x14.97 and x12.06 for x1 to x8.

### Where the per-event cost goes

What keeps these numbers where they are, so a change that loses one of them
shows up as a budget breach rather than a slow drift:

- **A late `type` costs nothing.** A union's discriminator is found by a
  lookahead that only steps over the members before it (no decoding, no
  UTF-8 check: the walk reads those bytes again), and the walk then presents
  it first, so the tagged visitor never buffers members as `(String, Value)`
  pairs. The `Value` path does the same by taking `type` out of the map first.
  (A sorted-key frame used to cost 4 extra allocations per event this way.)
- **Repeated ids are shared.** Ids are `Arc<str>`; the last few read on a
  thread are remembered, so `messageId` on every content delta and
  `toolCallId` on every argument delta allocate nothing (`ids.rs`).
- **A run of deltas hashes its id once.** The reducer and the consumer
  verifier remember the last content or argument delta's target (the
  table's own key, compared by pointer first) and forget it on any other
  event, so the next delta to the same item skips the SipHash lookup.
- **A closed small item keeps no slack.** `*_END` shrinks the text or
  arguments it closes when their buffer is at most 128 KiB, so the history
  holds what `max_history_bytes` charges for it (one shrinking reallocation
  per END, in the budgets above). Larger text keeps its growth slack, which
  is not resident (D14), so reopening a large item and appending to it does
  not copy the whole text again.
- **A finding nobody receives is not built.** With a `NullSink`
  (`DiagnosticSink::enabled` is `false`) or past the reporter's cap, the
  enforcer counts a stripped member without building its pointer and
  message, so the counts and summaries stay exact at no allocation.
- **Names are found by length and first byte.** Member and event-type
  tables are short and share long prefixes, so a scan that compares lengths
  and first bytes beats a binary search that compares the common prefix at
  every step.
- **Member names are matched as bytes.** A name without escapes is looked up
  in the shape table by its bytes; only an unrecognised one is checked as
  UTF-8. String scans move eight bytes a step (SWAR on a `u64`, no `unsafe`),
  as do the retired-marker check on every `push_json` frame and the SSE
  encoder's line-break check.
- **Per-run bookkeeping keeps its capacity** (cleared, not replaced), patch
  operation members are matched without allocating (`Cow<str>` keys always
  deserialize owned), the undo log is reserved once and its rare `move` entry
  is boxed (152 to 104 bytes an entry), and `PatchBudget::for_document`
  counts without a heap stack below 256 levels.
- **Typed events encode to protobuf without JSON text.** `encode_event`
  builds the encoder's tree straight from `Serialize` (strings copied
  unescaped into a reused arena, numbers as the `f64` `JSON.parse` would read)
  instead of writing JSON and parsing it back; the bytes are identical, and a
  value the builder does not mirror takes the text path.

## Measuring a change

Wall-clock numbers from a shared machine are not comparable between runs. A
change is measured instead by what load does not distort:

- **Instructions retired per event**: the process's count from
  `/usr/bin/time -l` (macOS; `perf stat -e instructions` on Linux) for a
  workload repeated `a` and `b` times, `(I(a) - I(b)) / (events(a) -
  events(b))`, so setup cancels. Repeat runs agree within about 2 %.
- **Allocations and bytes per event**, from the counting allocator (the
  bench budgets, exact).
- **Resident bytes per held item**: live bytes after a stream of N items,
  with the smallest `max_history_bytes` that still passes it as the
  estimate (binary search), from the same allocator.

0.9.1 against 0.9.0, measured that way on an Apple M-series laptop under
heavy load (instructions per event; the workloads are the bench streams):

| Path | 0.9.0 | 0.9.1 | Change |
| --- | ---: | ---: | ---: |
| `push_json`, typical stream (sorted keys, `type` last) | 8475 | 7769 | -8.3 % |
| `push_json`, the same with `type` first (TypeScript order) | 8304 | 7664 | -7.7 % |
| `push_json`, an unknown member on every content delta (`NullSink`) | 10604 | 8185 | -23 % |
| `push_value`, typical stream | 9392 | 8632 | -8.1 % |
| Reducer, content deltas to one message (event clone included) | 996 | 849 | -15 % |
| Reducer, the same with every update held | 1428 | 1272 | -11 % |
| Reducer, `TOOL_CALL_ARGS` to one call | 1005 | 856 | -15 % |
| Reducer, content deltas with two metadata keys (net of the clone) | 2784 | 1098 | -61 % |
| Reducer, two-operation `ACTIVITY_DELTA` (net of the clone) | 13929 | 9885 | -29 % |
| `MESSAGES_SNAPSHOT` reconcile, per message (10k, half held) | 3144 | 2765 | -12 % |
| Producer verifier (`server_emit_encode`'s check) | 450 | 457 | 0 % |
| `decode_event_json`, typical stream | 4622 | 4019 | -13 % |
| `decode_event` to a `Value` | 6225 | 6218 | 0 % |
| SSE decode, typical stream | 679 | 597 | -12 % |

Resident per held item: a text message streamed in 10 deltas (200 bytes)
794.6 to 674.6 bytes, a tool call with 200 bytes of streamed arguments 754.9
to 634.9 bytes; the history estimate goes from 0.83 to 0.98 of what is held
for streamed text. One unknown member per content delta with a `NullSink`: 5
to 1 allocations and 331 to 25 bytes per event.

## Allocation-budget policy

- **Every hot path has a written budget**, in the table above and asserted in
  the bench harness. "Per event" means steady state, after any buffers have
  grown to size.
- **The budget is a ceiling, and the gate enforces it exactly.** Allocation
  counts do not vary between runs, so there is no noise margin to hide in.
- **A change that raises a budget says so.** Moving a number is allowed, in the
  same pull request as the change that needs it, with the reason in the
  description and the changelog if users would notice. Lowering a budget after
  an optimisation is encouraged: it locks the gain in.
- **Peer-controlled sizes are bounded.** Any buffer that grows with peer input
  has a configurable limit, and exceeding it is an error, not an allocation.
- **Counting needs no `unsafe` in the library.** The counting global allocator
  lives in the bench/test harness (for example `dhat`'s testing mode or a
  small `GlobalAlloc` wrapper in the bench crate), never in a published crate,
  which stay `#![forbid(unsafe_code)]`.

## Profiling

### In CI

Dispatch **Actions → perf → Run workflow** with `profile`:

- `flamegraph`: a flamegraph SVG per bench target, uploaded as the
  `flamegraphs` artifact.
- `pgo`: a PGO build of the bench harness and the before/after delta in the
  run summary, with the logs as the `pgo-logs` artifact.
- `all`: both.

### Locally: flamegraph (Linux, macOS)

```sh
cargo install flamegraph
# Linux: needs `perf` and kernel.perf_event_paranoid <= 1.
# macOS: uses dtrace and needs sudo.
CARGO_PROFILE_BENCH_DEBUG=true cargo flamegraph --bench <bench> -o flame.svg -- --bench
```

`CARGO_PROFILE_BENCH_DEBUG=true` keeps symbols in the optimised build, so the
graph names functions rather than addresses.

### Locally: samply (Linux, macOS, Windows)

[samply](https://github.com/mstange/samply) records a sampling profile and
opens it in the Firefox Profiler, with call trees, inverted views and a
timeline. Often more useful than a static flamegraph for "why is this one call
slow".

```sh
cargo install samply
CARGO_PROFILE_BENCH_DEBUG=true cargo bench --no-run --bench <bench>
samply record target/release/deps/<bench>-<hash> --bench
```

`--profile-time <secs>` loops the bench's largest size with no gate, which
gives a steadier profile than one gated pass. On macOS samply needs no sudo
(cargo-flamegraph's dtrace does). To keep a profile for later, or to turn it
into a flamegraph, record with `--save-only --unstable-presymbolicate -o
<name>.json.gz`: the `.syms.json` sidecar resolves the frame addresses
without the binary, and the stacks fold into the format `inferno-flamegraph`
draws.

### Profile-guided optimisation (a downstream build option)

PGO is a build option for the *final binary*, not something this library
ships: a crate is compiled by its users, with their profile or none, so no
profile of ours reaches them. An application that embeds `agui-sdk` on a hot
path (a gateway relaying many streams, a server emitting many runs) can train
on its own traffic and rebuild; the crate needs nothing for that. Here, PGO
also tells us where better branch information would pay, which points at
branches worth restructuring and functions worth `#[inline]` or `#[cold]`
(then measured normally).

With [`cargo-pgo`](https://github.com/Kobzol/cargo-pgo), on the bench
harness:

```sh
rustup component add llvm-tools-preview
cargo install cargo-pgo
cargo pgo info                                 # llvm-profdata found?

cargo bench --no-run -p agui-sdk --benches     # 1. baseline binaries (target/release)
# 2. instrumented build, run as training (writes target/pgo-profiles/*.profraw;
#    the instrumented timings are meaningless, and the gate may fail on them)
cargo pgo bench -- --no-fail-fast -p agui-sdk --benches
# 3. merge the profiles and build with them (target/<host-triple>/release)
cargo pgo optimize bench -- --no-run -p agui-sdk --benches
# 4. run each pair of binaries alternately and compare the medians
target/release/deps/<bench>-<hash> --bench
target/<host-triple>/release/deps/<bench>-<hash> --bench
```

In an application the same three steps are `cargo pgo build`, running the
instrumented binary on representative traffic, and `cargo pgo optimize
build`. Without cargo-pgo, `RUSTFLAGS=-Cprofile-generate=<dir>`, `llvm-profdata
merge` and `RUSTFLAGS=-Cprofile-use=<file>.profdata` do the same (the CI
`pgo` job runs them that way).

Measured on an Apple M-series laptop (aarch64-apple-darwin, Rust 1.98),
trained on the benches themselves (the best case: the profile is the
workload), median ns per event, best of 5 alternating runs:

| Case | Release | PGO | Change |
| --- | ---: | ---: | ---: |
| `sse_decode/64KiB` | 22.9 | 21.8 | -5% |
| `proto_decode/framing` | 9.3 | 6.1 | -34% |
| `proto_decode/decode` | 249.6 | 191.8 | -23% |
| `pipeline_push_json/text_heavy` | 413.0 | 331.2 | -20% |
| `pipeline_push_value/text_heavy` | 401.6 | 324.2 | -19% |
| `reducer_deltas/one_message` | 39.0 | 26.7 | -32% |
| `reducer_deltas/one_message_held` | 59.9 | 51.6 | -14% |
| `messages_snapshot/reconcile` | 177.2 | 151.8 | -14% |
| `patch_apply/apply` | 409.4 | 368.1 | -10% |
| `patch_apply/failing` | 598.1 | 523.7 | -12% |
| `server_emit_encode/sse` | 92.1 | 72.0 | -22% |
| `server_emit_encode/proto` | 202.1 | 154.3 | -24% |
| `client_run/await` | 458.0 | 351.4 | -23% |

Allocation counts are unchanged (PGO moves code, not data). Typically 15 to
25 % per event; least where the work is memory-bound (`patch_apply` on a
1 MB document, SSE decode at 3 GB/s), most on short, branchy paths (frame
splitting, the reducer's delta append). The machine was shared and loaded
during the runs, so read single rows as +-5 %. Expect less when the training
traffic differs from production.

### Locally: allocations

`dhat` (heap profiling, through the `dhat` crate in a bench or test) answers
"which call site allocated this"; `heaptrack` does the same on Linux without
code changes.

## Checklist for a hot-path change

1. Run the relevant bench before and after.
2. If allocation counts moved, update the budget and say why.
3. If a buffer now grows with input, it has a limit.
4. Let the qa perf gate confirm on CI hardware.
