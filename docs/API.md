# agui-sdk API design

Status: implemented; this is the API of the 0.9.0 release. Scope: AG-UI 1.0,
client and server, SSE and protobuf. The crate's rustdoc (docs.rs) is the
normative reference for signatures; this file explains the design behind
them. Companion documents:

- `SPEC-MAP.md`: every gated ledger row mapped to its implementation and test.
- `TEST-PLAN.md`: test layout, corpus lanes, the gate, fuzzing and performance.

## 1. Principles

1. **One crate, features for weight.** `agui-sdk` is the only published
   library; `ag-ui-sdk` is a re-export alias. The default build is protocol
   types plus the sans-IO pipeline, with `serde` and `serde_json` as its only
   dependencies. Client, server, axum and protobuf are additive features.
   **Public dependencies are `serde` 1 (traits: derived impls on protocol
   types, and `Serialize`/`DeserializeOwned` bounds on typed state) and
   `serde_json` 1 (`Value`)**; no other third-party type is in a public
   signature (owner's rule; exceptions in §9: the opt-in `stream` and `axum`
   features implement third-party traits, which is their whole purpose).
2. **No unverified path.** Application code only ever sees events that went
   through the compatibility boundary, enforcement, chunk expansion and
   verification (PROC-31). The one place raw material is visible is
   middleware, before enforcement, as the spec requires (PROC-29).
3. **Verdicts live in one place.** The pipeline is a set of synchronous state
   machines (`pipeline::*`) with no runtime and no I/O. The client, the
   server's producer verifier, the corpus tests and any custom host all call
   the same code, so they reach the same verdicts (PROC-06).
4. **Producers cannot emit an invalid stream.** The server runner owns the
   run lifecycle and checks every event before it reaches the encoder
   (ARCH-04). A violation surfaces as an `Err` at the agent's call site.
5. **Never silent, never noisy.** Warnings the spec requires go to a sink that
   is on by default (VER-11). A conformant stream produces none (PROC-37).
6. **Additive evolution.** Every public protocol struct and enum is
   `#[non_exhaustive]`, built through constructors and `with_*` setters, so a
   protocol minor is a minor crate release (VER-01).
7. **Performance is part of the contract.** Hot paths have stated complexity
   and allocation budgets (§8), and CI enforces them (TEST-PLAN §9).

## 2. Module tree

```
agui_sdk
├── PROTOCOL_VERSION, Error, ErrorCode, ErrorKind, Result, BoxError, Event, EventType, Message, RunAgentInput
├── prelude        common imports (protocol types; client/server items per feature)
├── ids            ThreadId, RunId, MessageId, ToolCallId, SubagentRunId, InterruptId   (Arc<str> newtypes)
├── json           JsonObject (open object), Iter
├── metadata       Metadata = JsonObject, RESERVED_KEY, vendor_key, MetadataBuilder (insert, try_insert), provider::*
├── event          Event (31 variants, size asserted), EventType; re-exports every event struct
├── lifecycle      RunStarted (input boxed), RunFinished, RunError, StepStarted, StepFinished, RunFinishedOutcome
├── text           TextMessageStart/Content/End/Chunk, TextMessageRole
├── tool           ToolCallStart/Args/End/Chunk/Result, Tool, ToolCall, FunctionCall, ToolRole, ToolCallType
├── state          StateSnapshot, StateDelta, MessagesSnapshot
├── activity       ActivitySnapshot, ActivityDelta
├── reasoning      ReasoningStart/End, ReasoningMessageStart/Content/End/Chunk, ReasoningEncryptedValue, ...
├── subagent       SubagentStarted/Finished/Error, SubagentFinishedOutcome
├── passthrough    Raw, Custom
├── message        Message, Role, Developer/System/Assistant/User/Tool/Activity/ReasoningMessage
├── content        Content (text | parts), ContentPart, MediaPart, TextPart, PartSource, Data/Url/FileSource
├── run_input      RunAgentInput (tools/context/resume: Option<Vec<_>> + slice accessors), Context, SendableMessages
├── interrupt      Interrupt, ResumeEntry, ResumeStatus, Expiry
├── usage          TokenUsage, TokenUsageBuilder
├── capability     AgentCapabilities (+11 groups), SubagentInfo, Support, ingest*, CapabilitiesBuilder
├── patch          JsonPointer, PatchOperation (hand-written serde), PatchOp, PatchLimits, PatchBudget, apply, diff, PatchError
├── version        ProtocolVersion (new), PeerVersion, LegacyEra, PeerPin
├── wire           WireFormat, SSE/PROTO/JSON media types, negotiate, client_accept, Chunk [client|server|proto]
├── cancel         CancelToken, Cancelled   (std only, waker-capable; client and server)
├── diagnostics    Diagnostic, DiagnosticCode, Scope, DiagnosticSink, DefaultSink, NullSink,
│                  CollectingSink, Tee, default_sink, Reporter
├── error          Error (8 B, boxed), ErrorCode, ErrorKind, Result, BoxError
├── pipeline       Limits, PipelineConfig, Processor (owns Boundary + EraShims), Processed, Termination
│   ├── compat     Boundary, EraShims (upgrade_event, downgrade_event, downgrade_input)
│   ├── enforce    Enforcer (check_outgoing_input, enforce_outgoing_value, ...), validate_strict, enforce_document
│   ├── chunks     ChunkExpander
│   ├── verify     ConsumerVerifier (expecting), ProducerVerifier (for_input), ProducerRule, ProducerViolation, RunPhase
│   └── reduce     Reducer (apply takes Event by value), Changes, PendingToolCall
├── proto          [proto] MEDIA_TYPE
│   ├── encode     encode_event, encode_value, encode_json
│   ├── decode     decode_event, decode_event_json, Decoded, DecodedJson
│   └── framing    write_frame, FrameDecoder, DEFAULT_MAX_FRAME_BYTES
├── client         [client] AgentClient, ClientBuilder, RunRequest, RunHandle, RunFuture, RunUpdate,
│   │              RunReport, RunStatus, Observer, RunView, RunInfo, Thread, ThreadSnapshot, ApprovedToolCall,
│   │              SchemaCheck, DecidedBy, Clock, CancelToken, (re-exports middleware items, PendingToolCall)
│   ├── middleware Frame, FrameSource, FrameStream, map_frames, Middleware, MiddlewareRequest, Next
│   ├── transport  Transport, DynTransport (sealed), BoxTransport, TransportRequest, Header, HttpTransport,
│   │   │          HttpTransportBuilder, HttpLimits, RetryPolicy, TlsRoots, TlsBackend, json_frame
│   │   └── sse    SseDecoder, DEFAULT_MAX_EVENT_BYTES
│   ├── capabilities  CapabilitiesRequest (performed by HttpTransport::fetch_capabilities)
│   └── subscriber Subscriber, Flow, Mutation
└── server         [server] Agent, DynAgent (sealed), BoxAgent, BoxError, RunOutcome, RunContext, Runner, RunnerConfig,
    │              RunStream, Transformer, CancelToken, Cancelled, ReplayedRun, Extensions
    ├── emit       Emitter (incl. record_usage), TextStream, ToolCallStream, ReasoningSpan, ReasoningStream, StepSpan, SubagentScope
    ├── input      admit, admit_input, InputPolicy, AdmittedInput
    ├── state      StatePublisher
    ├── helpers    PartFilter (filter_messages: run-input pointers; filter_message, filter_result), context_prompt,
    │              flatten_for_legacy_peer, InterruptStore (trait), MemoryInterruptStore, ResumeCheck
    ├── encode     Encoder; sse::SseEncoder, json::NdjsonEncoder, proto::ProtoEncoder [proto]
    ├── http       handle, HttpRequest, HttpResponse, ResponseBody (yields Chunk), HttpRejection, ServeConfig,
    │              capabilities_response
    └── axum       [axum] AguiRequest (FromRequest), AguiResponse (IntoResponse), respond
```

Workspace: `crates/ag-ui-sdk` (alias), `crates/test-support` (unpublished
harness), `xtask` (unpublished: spec fetch, codegen, drift, trace, MSRV lock).

## 3. Naming conventions

- **Module = spec area.** `lifecycle`, `text`, `tool`, `state`, `activity`,
  `reasoning`, `subagent` and `passthrough` match `events/*.mdx`;
  `run_input`, `capability`, `metadata`, `interrupt` and `version` match
  `basic/*.mdx`. A reader of the spec page knows which module to open.
- **Event structs drop the schema's `Event` suffix**: `$defs/RunErrorEvent` is
  `lifecycle::RunError`, and the variant is `Event::RunError`. Every type's
  rustdoc names its `$defs` entry. `Raw` and `Custom` keep the schema's short
  names.
- **Roles, not frameworks.** `AgentClient` calls an agent; `server::Agent` *is*
  one. In a single crate the TypeScript name `HttpAgent` would collide with
  the server trait, so it is gone.
- **Builders.** Protocol types: `new(required…)` plus `with_<field>(value)`.
  Configuration types: infallible setters plus `build() -> Result`. The
  protocol-side builders that compute a value from parts
  (`MetadataBuilder`, `TokenUsageBuilder`) have an infallible `build()`,
  because every input they accept is valid; `CapabilitiesBuilder::build`
  returns `Result` because it validates the declaration (CAP-03). Entry
  points: `HttpTransport::builder(url)`, `TokenUsage::builder(provider,
  model)` (the same as `TokenUsageBuilder::new`) and
  `AgentCapabilities::builder()` (the same as `CapabilitiesBuilder::new`).
- **`#[must_use]`.** Builders, lazy runs (`RunHandle`, `RunFuture`,
  `RunStream`, `ResponseBody`) and every setter or accessor whose only effect
  is its return value are `#[must_use]`. The crate enables
  `clippy::must_use_candidate` and `clippy::return_self_not_must_use`, which
  catch most new candidates but not macro-generated items or every accessor;
  those are marked by hand. Item helpers on the server: `start_*` returns a
  handle and `end`/`finish` (or `fail`) consumes it; the handle types are
  `#[must_use]`, since an item whose handle is dropped stays open until the
  run's closers close it.
- **Verbs.** `admit` (server input), `ingest` (capabilities), `enforce`
  (strip and reject), `observe`/`check` (verify), `apply` (reduce, patch),
  `encode`/`decode` (wire).
- **Rule ids in rustdoc.** Every item that serves a rule cites it, e.g.
  `(LIFE-41)`. `SPEC-MAP.md` is the reverse index.

## 4. Pipelines

### Consumer (client), per received frame

```
 HTTP body ──▶ SseDecoder ─┐                       (bytes, zero-copy slices)
           └─▶ FrameDecoder + proto::decode ─┐      (unknown arm → drop + warn, PB-17)
                                            ▼
                                 Frame (raw JSON bytes | Value)
                                            │
                       ┌────────────────────┴───────────── no middleware installed ──────────────┐
                       ▼                                                                         │
             compat::Boundary  (null→absent, THINKING_*→REASONING_*, legacy binary; PROC-22)      │
                       ▼                                                                         │
             compat::EraShims  (only for a pinned 0.0.x peer; VER-14)                             │
                       ▼                                                                         │
             Middleware chain  (raw JSON, first registered = outermost; PROC-21, PROC-29)         │
                       ▼                                                                         ▼
             Processor::push_value                                   Processor::push_json (one parse; own Boundary + EraShims)
                       └───────────────────────────────┬────────────────────────────────────────┘
                                                       ▼
                                 enforce::Enforcer   (drop/strip + warn, malformed = fatal; PROC-01..20)
                                                       ▼
                                 chunks::ChunkExpander  (lanes per owner and kind; STRM-07..27)
                                                       ▼
                                 verify::ConsumerVerifier (lifecycle, triads, ownership; LIFE/TEXT/TOOL/REAS/SUB)
                                                       ▼
                                 reduce::Reducer  (takes the Event by value: history, state, metadata merge, pending calls)
                                                       ▼
                    Observers (sync, borrowed RunView + RunInfo) → RunHandle::next item (RunUpdate { Arc<Event>, changes })
                                                       ▼
                          end of body: Processor::finish_checked → Complete | Truncated (TRN-09) | verify.identity (RUNIN-06)
```

### Outgoing input (client), once per run

```
RunRequest::into_input → Thread checks (resume coverage/expiry, pending calls; INTR-11, TOOL-33)
  → strip activity messages (RUNIN-12) → Enforcer::check_outgoing_input (typed, no Value round trip; PROC-34)
  → declare protocolVersion "1.0" unless the peer is pinned pre-versioning (RUNIN-08)
  → no middleware: serialize once → Transport::open(TransportRequest { body: Chunk, ids, headers, Accept, cancel })
  → with middleware: to Value once → EraShims::downgrade_input (pinned legacy peer) → middleware sees
    MiddlewareRequest.input → Enforcer::enforce_outgoing_value (strip + warn, malformed = fatal; PROC-32..34)
    → serialize → Transport::open
```

### Producer (server), per request

```
HTTP request ─▶ http::handle / axum::respond: negotiate Accept (SSE unless protobuf is admitted) → Content-Type (415) → input::admit (400/413)
            ─▶ Runner::run (state must fit A::State, else 400; Extensions → RunContext) ─▶ RUN_STARTED (own protocolVersion; optional input echo)
                 agent.run(RunContext) ── Emitter::emit ─▶ attribution ─▶ Transformers
                                                        ─▶ ProducerVerifier::for_input(..).check (refuse → Err at call site)
                                                        ─▶ StatePublisher::observe
                                                        ─▶ bounded queue (await when full)
                 outcome / Err / panic / overflow ─▶ closers (LIFE-12) ─▶ exactly one terminal
            ─▶ EraShims::downgrade_event (RunnerConfig::legacy_peer, undeclared clients only)
            ─▶ ResponseBody: Encoder (SSE LF-only + keep-alives | protobuf frames) ─▶ coalesced Chunk per poll
```

## 5. Errors

One type, `agui_sdk::Error`, for every fallible operation (client, server,
pipeline, codec) except the outcomes listed at the end of this section. It is opaque, with accessors:

- `code() -> ErrorCode`: a stable dotted string (`"verify.tool_call"`),
  `#[non_exhaustive]`, never reused. It has a `kind()` coarse class:
  `Usage | Transport | Protocol | Truncated | Conversation | Cancelled |
  Extension | Server | Internal`.
- `message()`: human text with no payload values and no secrets.
- `pointer()`: the JSON pointer of the offending value, for validation errors.
- `http_status()` and `retry_after()` for pre-stream HTTP failures.
- `is_retryable()`: only failures before response headers (connect,
  including a TCP, proxy or TLS-handshake timeout; 429, 502, 503, 504).
  Response, idle and total timeouts are not. It implies `!run_started()`.
- `run_started()` and, with `client`, `partial()`/`into_partial()`: the
  `RunReport` of everything delivered before the failure (LIFE-19, TRN-11).
- `source()` reaches the underlying cause. No third-party error type appears
  in a signature.

The codes are listed in `src/error.rs` (`error_codes!`); the table there is the
contract. Families: `config.*`, `request.*`, `transport.*`, `framing.*`,
`processing.*`, `verify.*`, `stream.truncated`, `resume.*`, `thread.*`,
`run.cancelled`, `observer.panicked`, `middleware.failed`,
`capabilities.rejected`, `agent.*`, `server.*`, `internal.*`.

Two outcomes are deliberately **not** errors:

- A producer's `RUN_ERROR` is a delivered outcome: `RunStatus::Errored` in an
  `Ok(RunReport)`. A stream the pipeline rejects is an `Err`. The two are
  different types, so they cannot be confused (LIFE-54).
- A JSON Patch that fails to apply is a `PatchError` plus a
  `patch.not_applied` diagnostic, and the run continues (SNAP-12, SNAP-13).

Four values are verdicts handed back for the caller to act on, not failures
of the call: `ProducerViolation` (`ProducerVerifier::check`'s refusal of one
event), `HttpRejection` (`server::http::handle`'s pre-stream refusal, which
carries the HTTP status to send), the `Diagnostic` from
`MetadataBuilder::try_insert` (`metadata.reserved_key`, for the caller to
report), and `JsonObject::from_value` handing back the non-object `Value`
unchanged.

## 6. Diagnostics

`Diagnostic { code, scope, pointer, event_type, unknown_type, run_id, message }`, with
`DiagnosticCode` a stable dotted string: `drop.*`, `strip.*`, `compat.*`,
`downgrade.lossy`, `version.*`, `patch.*`, `snapshot.*`, `activity.missing_target`,
`stream.conflicting_reopen`, `stream.missing_target`, `reasoning.*`, `resume.unknown_entry`,
`interrupt.unparseable_expiry`, `content.*`, `metadata.reserved_key`,
`capabilities.ignored`, `diagnostics.limit`. The full list is in
`src/diagnostics.rs`.

Routing (`DefaultSink`): `tracing` at WARN on target `agui_sdk::diagnostics`
when feature `tracing` is on and a subscriber is interested; otherwise `log`
when feature `log` is on and a logger is set; otherwise standard error. Only
an explicit `NullSink` silences (VER-10, VER-11). `Reporter` caps diagnostics
per event (default 256) and emits one `diagnostics.limit` summary. Its
`report(|| …)` takes a closure, so nothing is allocated once the cap is hit,
when there is nothing to say, or when the sink is not
`DiagnosticSink::enabled` (`NullSink`; the counts and caps still move as if
delivered). Client runs also collect diagnostics into
`RunReport::diagnostics` and pass each to `Observer::on_diagnostic`.

## 7. Extension points

| Point | Side | Shape | Sees | Can |
|---|---|---|---|---|
| `client::transport::Transport` | client | RPITIT `open(TransportRequest) -> Future<Result<FrameStream>>`; sealed `DynTransport`/`BoxTransport` for `dyn`; `TransportRequest::new` for tests | serialized input (`Chunk`), thread/run ids, per-run headers, `CancelToken`; produces raw frames | carry AG-UI over any channel (TRN-12, TRN-13); a panic while opening or reading is contained (`transport.stream_read`) |
| `client::middleware::Middleware` | client | sync `run(MiddlewareRequest, Next) -> FrameStream`; closures implement it; `map_frames` helper | input JSON and raw frames, before enforcement | rewrite, drop, insert, delay |
| `client::Observer` | client | sync, default no-ops: `on_event(update, view)`, `on_diagnostic(d, run)`, `on_finish(result, run)` | verified events, a borrowed `RunView`, the run's `RunInfo` | read only; panics are contained |
| `client::subscriber::Subscriber` | client | closures keyed by `EventType` returning `Flow` | verified events | replace state/history, stop propagation |
| `diagnostics::DiagnosticSink` | both | sync `report(&Diagnostic)`; closures implement it | every diagnostic | route, count, silence |
| `server::Agent` | server | RPITIT `run(RunContext<State>) -> Future<Result<RunOutcome, BoxError>>`; sealed `DynAgent`/`BoxAgent` | admitted input, typed state, request `Extensions` | emit through `Emitter`; choose `RUN_ERROR` via `RunOutcome::failed` or `Error::public` |
| `server::helpers::InterruptStore` | server | RPITIT async `record`/`check`/`resolve`; `MemoryInterruptStore` bounded | raised interrupts per thread | back the INTR-18..23 checks with any storage |
| `server::Transformer` | server | sync `transform(Event, &mut Vec<Event>)`, one per run via factory | agent events, before verification | redact, translate, expand |
| `server::http::handle` | server | sans-IO request → `HttpResponse` or `HttpRejection` | request parts | glue any HTTP framework |
| `pipeline::Processor` and the stages | both | sync state machines | anything | build custom hosts and tools |

Every async trait uses return-position `impl Future + Send` (stable since
1.75), with a blanket boxed twin for trait objects. No `async-trait`.
Observers, middleware, sinks and transformers are synchronous and object-safe.
The boxed twins (`DynTransport`, `DynAgent`) are sealed; the open traits
(`Transport`, `Agent`, `Middleware`, `Observer`, `Transformer`,
`DiagnosticSink`, `InterruptStore`) only ever gain methods with default bodies.

Async streams are the crate's own types (`RunHandle`, `FrameStream`,
`RunStream`, `ResponseBody`) with inherent `next()` and `poll_next()`, so no
`futures` crate is needed to consume them. Feature `stream` adds
`futures_core::Stream` impls and `FrameStream::from_stream`.

## 8. Hot paths: complexity and allocation budgets

These budgets are acceptance criteria. `TEST-PLAN.md` §9 says how CI
enforces them. *n* = bytes in,
*d* = delta size, *k* = members touched.

| Path | Complexity | Allocation budget (steady state, per event) | How |
|---|---|---|---|
| SSE decode (`SseDecoder`) | O(n), one scan per byte | 0 for a single-line event within one chunk (a `Chunk` slice); 0 amortized otherwise (reused assembly buffer) | `wire::Chunk` slicing (shared storage); capacity reuse |
| Proto framing (`FrameDecoder`) | O(n) | 0 when the frame lies in one chunk; amortized 0 otherwise | `Chunk` slices; size checked before buffering |
| Parse + boundary + enforce, no middleware (`Processor::push_json`) | O(n) single pass | one allocation per owned string/array the typed event keeps; no intermediate `Value` | shape-table-driven `DeserializeSeed` |
| Enforce after middleware (`push_value`) | O(n) | 0 beyond the typed event (strings moved out of the `Value`) | in-place strip, then move |
| Middleware frame peek (`Frame::event_type`) | O(prefix) | 0 | scan, no parse |
| Chunk expansion | O(1) | 0 (lane table reused; ids are `Arc<str>` clones) | fixed per-owner table |
| Consumer/producer verification | O(1) expected | 0 amortized (hash sets of `Arc<str>`) | |
| Reducer: every event (`Reducer::apply`) | O(1) | one `Arc<Event>` per event, which `Processed.event` and `RunUpdate.event` share; 0 for an event that follows a content delta whose update the application has already dropped | `apply(Event)` by value, wrapped once; the latest content delta's `Arc` is recycled |
| Reducer: content delta | O(1) lookup + O(d) append | amortized `String` growth of the message only (≤ log2(total bytes) over a message's life) when updates are dropped promptly; plus one `Arc` per delta when the application holds them | id → index map; no history clone |
| Reducer: `MESSAGES_SNAPSHOT` | O(held + snapshot) | one copy of the snapshot's messages (the history lends `&[Message]` while the event keeps its own) | one index rebuild |
| Reducer: `STATE_SNAPSHOT` | O(1) to install | 0 copies when the application drops the update before the state next changes; else one copy-on-write clone at that change | the state is read through the shared event and taken with `Arc::try_unwrap` |
| Reducer: `ACTIVITY_SNAPSHOT`, tool result, metadata | O(payload) | one copy of the payload into the history | |
| JSON Patch apply (`STATE_DELTA`, `ACTIVITY_DELTA`) | O(Σ op size + depth) | the undo log, plus one copy of each operation value installed | in-place apply, undo on failure; incremental node budget (node units: one per value plus one per 64 bytes of string or key text, so a `copy` is charged for the text it clones); a unit is 32 B (scalar array element) to about 720 B (single-member object with a key under 64 bytes) resident, and the default ceiling of 180,000 units holds the patched state and activity content to about 124 MiB (DECISIONS.md D14; see "Client memory and large state" below) |
| `RunUpdate` delivery | O(1) | 0 beyond the reducer's per-event `Arc` (never a deep clone) | observers borrow `RunView` |
| Outgoing input, no middleware | O(input) | one serialization, no `Value` round trip | `check_outgoing_input(&RunAgentInput)` |
| Outgoing input, with middleware | O(input) | one `Value` conversion plus one serialization | `enforce_outgoing_value(&mut Value)` in place |
| Server emit | O(1) | 0 beyond the event | verifier check plus one queue slot |
| SSE/NDJSON/proto encode | O(event) | 0 amortized (caller-owned buffer, reused) | `Encoder` enum, no `dyn` |
| `ResponseBody` chunks | O(ready events) | 0 amortized: every ready event coalesced into one `Chunk` split off a reused buffer | split-and-freeze of a reused buffer, internally |
| Diagnostics, nothing to report | O(1) | 0 | lazy closure |

Why the `Arc` is recycled only after content deltas: reusing an allocation
needs the reducer to keep a handle to the previous event until the next one
arrives, so while it does, the application's handle is not unique
(`Arc::try_unwrap` on it fails). Deltas are the hot path and carry nothing
worth taking, so they are kept; snapshots, results, custom and raw events
are not, so the application can always take those payloads without a copy.
Handing the event out by value instead would change `Processed` and
`RunUpdate`, and a pool would be the same trade with more state.

### Client memory and large state

Under `Limits::default()`, worst-case resident client memory for one stream
stays near 252 MiB (64-bit; DECISIONS.md D14): about 124 MiB of state and
patched activity content (the `PatchLimits::max_nodes` ceiling of 180,000
node units, at about 720 B per unit in the worst shape) plus 128 MiB of
history and id bookkeeping growth (`Limits::max_history_bytes`, charged in estimated resident
bytes). Not included: the event being parsed (at most
`Limits::max_container_entries`, 150,000 entries, about 103 MiB in the worst
shape, plus its text), the one event that crosses the history limit, and
history restored with `Processor::with_history` or kept by a `Thread` across
runs, which is the host's to trim.

An application with large state raises the limit it needs:

- `PatchLimits::max_nodes`: each extra node unit may hold up to about 720 B
  (about 140 B for objects with short keys, 32 B for a scalar array element),
  so 1,000,000 units allow up to about 690 MiB of state. The ceiling does not
  change the per-patch walk budget, which follows the document's size. Raise
  `Limits::max_container_entries` with it, below `max_nodes`, to admit a
  `STATE_SNAPSHOT` of that size; each entry may cost up to about 720 B while
  the event is parsed, and the same limit bounds server input admission,
  `validate_*` and the protobuf decode budget.
- `Limits::max_history_bytes`: each extra byte is about one more byte of
  resident history (the estimate stays within 0.9x to 1.4x of what is
  allocated). `usize::MAX` disables it; the reconcile work bound on
  `MESSAGES_SNAPSHOT` still applies.

Diagnostics are bounded per event (`max_diagnostics_per_event`) and in the
client report (`max_report_diagnostics`), not per stream: their volume over a
stream is linear in the input, which the limits above already bound, and a
per-stream sink cap would hide later problems (DECISIONS.md D21).

Type sizes (64-bit): `Event` is 200 B with serde_json's default map (target,
checked by a unit test in `src/event.rs`) and 320–336 B when `preserve_order` is unified
into the build; a compile-time assertion in `src/event.rs` caps it at 384 B.
`Error` is 8 B (asserted). `RunStarted.input`
is boxed so the rare echo does not size every event.

Places that box on purpose, once per run rather than per event: the
`FrameStream` of a transport and of each middleware, the run driver, and the
server's `RunStream`. Per event there is one virtual call per observer. The
alternative, generic observer lists, would make `AgentClient` generic over
every observer type.

## 9. Dependencies

Default build: `serde`, `serde_json`. Every other dependency is optional, and
none enables a default feature: those with defaults set
`default-features = false`; the rest have no default features.

| Crate | Features | Behind | Why | Alternatives considered |
|---|---|---|---|---|
| `serde` 1.0.200 | `std`, `derive`, `rc` | always | Wire types derive (de)serialization; `rc` lets ids be `Arc<str>`. | Hand-written impls for ~100 types: error-prone, no gain. |
| `serde_json` 1.0.120 | `std` (+ `preserve_order` via our `preserve-order`) | always | Parser and `Value` for open JSON. `preserve_order` only through the opt-in crate feature (D4); it unifies across the build, so it is never on by default. | simd-json, sonic-rs: `unsafe`, arch/nightly constraints. |
| `bytes` 1.11.1 | `std` | client, server, proto | Private backing storage of `wire::Chunk`: zero-copy slicing, and what hyper and axum-core use (no conversion copy). In no signature. | `Vec<u8>` copies: break the zero-copy budget. |
| `memchr` 2.7 | `std` | client | SIMD line-end search in the SSE decoder; already in the default tree via serde_json. | a byte loop. |
| `pin-project-lite` 0.2.14 | — | client, server | Safe pin projection in hand-written futures and streams. | `pin-project` (proc-macro). |
| `hyper` 1.4 | `client`, `http1` | client | HTTP/1.1 client (D1). No HTTP/2: SSE over HTTP/1.1 is the norm; `h2` adds indexmap/hashbrown and h2 advisories. | reqwest 0.12: 93 vs 47 deps, url/idna/ICU, phantom quinn-proto lock entries (`cargo tree` at the time of D1). |
| `hyper-util` 0.1.7 | `client-legacy`, `http1`, `tokio` | client | Pool and connector. Pulls `tracing` unconditionally. | — |
| `tower-service` 0.3 | — | client | The connector trait hyper-util's pool is generic over (proxy tunnel, TLS, connect timeout). Already in hyper-util's graph. | — |
| `http` 1.1, `http-body` 1, `http-body-util` 0.1.2 | `std` | client, axum | Request/response plumbing for hyper and axum-core, internal only. | — |
| `tokio` 1.44.2 | `time`, `net` | client, axum | Connect, response-header, idle and total timeouts, retry backoff, SSE keep-alives (axum); `net` for the proxy `CONNECT` handshake (hyper-util enables it anyway). `rt` comes via hyper-util: the client needs a tokio runtime. `server` alone does **not** use tokio. | runtime-agnostic timers: hyper-util needs tokio anyway. |
| `hyper-rustls` 0.27.3, `rustls` 0.23.46, `webpki-roots` 1.0.10 | ring, tls12, webpki-tokio | rustls | HTTPS with ring (no C toolchain) and the Mozilla bundle; floors clear 2024–2026 advisories. Roots and client identity are taken as PEM/DER bytes: no rustls type is public. | aws-lc-rs: +8 build deps and a C toolchain; users may still install it as the process default provider. |
| `rustls-native-certs` 0.8 (also via `hyper-rustls/native-tokio`) | — | rustls-native-roots | OS trust store for `TlsRoots::Native`, named directly so it combines with added roots. | — |
| `hyper-tls` 0.6, `native-tls` 0.2.12, `tokio-native-tls` 0.3 | — | native-tls | Platform TLS. Needs Rust 1.80: the advisory-clean openssl 0.10.80+ on Linux (§10). | — |
| `axum-core` 0.5.2 | — (none by default) | axum | `FromRequest`/`IntoResponse` for axum 0.8 apps (D2): 23 vs 41 deps. 0.5.2 is the last axum-core on 1.75. | `axum` 0.8: 41 deps, ~3× build time; routing belongs to the application. |
| `futures-core` 0.3.30 | `std` | stream | `Stream` impls for the crate's own streams. The only feature that makes a futures-core item public. | inherent `next()`/`poll_next()` by default. |
| `tracing` 0.1.40 | `std` | tracing | Diagnostics routing. | — |
| `log` 0.4.20 | `std` | log | Diagnostics routing for `log` users. | — |

**Public-dependency policy.** `serde` 1 and `serde_json` 1 are public
dependencies: `serde_json::Value` appears in signatures, protocol types
implement serde's `Serialize`/`Deserialize`, and typed-state APIs
(`server::Agent::State`, `RunRequest::state`, `RunReport::state_as`) take
serde trait bounds. A major of either would be our major. No other
third-party type appears in a public signature.
Two opt-in features exist to implement third-party traits and are public
dependencies only while enabled: `stream` (futures-core 0.3) and `axum`
(axum-core 0.5, i.e. axum 0.8). A new major of either arrives as a new
feature; existing feature names never change meaning.

Deliberately absent:

- `thiserror`: `Display` is a few lines by hand.
- `async-trait`: RPITIT plus boxed twins instead.
- `uuid`: ids are strings; `random()` mints 128-bit hex from std's seeded
  hasher and a counter.
- `json-patch`: our bounded, atomic, undo-log patch is simpler and enforces
  the node budget incrementally (in node units: values plus text at one
  unit per 64 bytes).
- `prost` / `prost-types`: a hand-written wire codec over a generated field
  table. It keeps JS key order for `Struct`, which prost-types cannot, and
  costs zero dependencies.
- `chrono` / `time`: a hand RFC 3339 reader for `expiresAt`.
- `pin-project`: one `Pin<Box<…>>` per run.
- `mime`: media types are matched by hand (exact type match, PB-07).
- `reqwest`: replaced by hyper (D1).
- `futures-util`: `poll_fn`/`next` are hand-written (it is still compiled
  under `client`, through hyper-util, but never public).

## 10. MSRV

**Rust 1.75 for the whole crate, every feature included except
`native-tls`: 1.80**, on edition 2021. 1.75 is the floor that RPITIT and
`async fn` in traits require; 1.80 is what the advisory-clean `openssl`
0.10.80+ / `openssl-sys` 0.9.115+ require (docs/DECISIONS.md D1).

- Dependency requirements stay loose enough that 1.75-compatible releases
  satisfy them; newer toolchains resolve to the latest versions.
- **The committed `Cargo.lock` is the MSRV lock** (lockfile `version = 3`),
  written by a cargo ≥ 1.84 with `CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback
  cargo update`, then `cargo update -p openssl -p openssl-sys` to their
  advisory-clean releases (openssl 0.10.82, openssl-sys 0.9.118, Rust 1.80).
  It resolves hyper-util 0.1.20, tokio 1.53.3, hyper-rustls 0.27.7,
  axum-core 0.5.2, native-tls 0.2.13, zeroize 1.8.2. Every locked crate
  needs at most 1.75 except four reached only through `native-tls`: openssl
  and openssl-sys (1.80), and the wasm-only wasip2 (1.87) and wit-bindgen
  (1.85) under `tempfile`. Verified: `cargo +1.75 check --locked` with every
  feature but `native-tls`, and `--no-default-features`;
  `cargo +1.80 check --all-features --locked`; on stable, `check --all-features`, `clippy
  --all-features -D warnings`, and `doc` with `-D warnings` for
  `--all-features`, `--no-default-features` and each feature alone.
- **Downstream users on 1.75**: cargo 1.75 has no MSRV-aware resolver, and a
  fresh `cargo +1.75 generate-lockfile` picks releases that need 1.80–1.85
  (some with edition-2024 manifests 1.75 cannot parse). Generate the lock with
  a cargo ≥ 1.84 and the fallback resolver, then build with `cargo +1.75
  --locked`. With `native-tls`, do not run `cargo +1.75 fetch`/`vendor`
  without `--target` (a wasm-only crate in the lock has a 2024 manifest).
- CI's MSRV job regenerates the lock with the fallback resolver (`cargo
  xtask msrv-lock`) and reports drift (the openssl pair always drifts down;
  that part is not committed), on Linux, macOS and Windows, and checks the
  library with no features there with warnings denied. On the committed lock,
  on Linux, it checks the library with each feature alone (`native-tls` on
  1.80, the rest on 1.75) with warnings denied and tests each on top of the
  test harness's `client`, `server` and `proto`; it checks and tests every
  feature but `native-tls` together on 1.75, and `--all-features` on 1.80.
  Clippy runs on stable with `clippy.toml` `msrv = "1.75"`
  (`clippy::incompatible_msrv`), for the workspace and for the library alone
  with no features and each feature (D15).
- **No advisory exceptions.** The 1.75-compatible `openssl` 0.10.78 carries
  RUSTSEC-2026-0474, -0476 and -0477, so the lock holds the patched releases
  and `native-tls` needs 1.80 instead; `deny.toml` ignores nothing. The rustls
  graph is advisory-clean; with no reqwest there are no phantom `quinn-proto`
  entries.
- Code rule: no let-chains, no `impl Trait` in associated types, no
  `#[diagnostic]` attributes, nothing stabilised after 1.75. Traps:
  `Result::inspect_err` (1.76), `Option::is_none_or` (1.82), `LazyLock`
  (1.80; use `OnceLock`), `std::iter::repeat_n` (1.82), `core::error::Error`
  (1.81).
- Raising the MSRV is a minor-release change, announced in the changelog,
  and only for a concrete need.

## 11. Features

| Feature | Enables | Extra deps |
|---|---|---|
| *(none)* | protocol types, diagnostics, errors, patch, version, wire, cancel, sans-IO pipeline | serde, serde_json |
| `client` | `client` (plain `http://`; needs a tokio runtime) | hyper, hyper-util, http, http-body(-util), tokio(time, net), tower-service, bytes, memchr, pin-project-lite |
| `rustls` | HTTPS through rustls (ring, webpki-roots); implies `client` | hyper-rustls, rustls, webpki-roots |
| `rustls-native-roots` | OS root store too; implies `rustls` | rustls-native-certs |
| `native-tls` | HTTPS through the platform stack; implies `client` | hyper-tls, native-tls, tokio-native-tls |
| `server` | `server` (runtime-agnostic) | bytes, pin-project-lite |
| `axum` | `server::axum` (`AguiRequest`, `AguiResponse`, `respond`); implies `server` | axum-core, http, http-body(-util), tokio(time, net) |
| `proto` | `proto`; protobuf in client/server when they are on | bytes |
| `stream` | `futures_core::Stream` impls for the crate's streams | futures-core |
| `preserve-order` | `serde_json/preserve_order` (D4; unifies across the build) | indexmap (via serde_json) |
| `tracing` | diagnostics → tracing | tracing |
| `log` | diagnostics → log | log |

The default set is empty (it was `["rustls"]`); `features = ["rustls"]` is
the usual client choice. With both TLS features on (`--all-features`), rustls
is used unless `HttpTransportBuilder::tls_backend(TlsBackend::NativeTls)`.
With neither, `build()` refuses `https://` URLs with `config.invalid`.
`client` deliberately implies no TLS backend (DECISIONS.md D11), so every
install line in the README and rustdoc uses `features = ["rustls"]`.

The rustdoc of `src/lib.rs` carries the same table.

## 12. Spec area → module

| Spec page | Module(s) |
|---|---|
| `index.mdx` (security) | `client::Thread::approve`, `client::ApprovedToolCall`, rustdoc |
| `architecture.mdx` | `pipeline` (order), `server::Runner` (strict writer) |
| `basic/index.mdx` (event model) | `event`, `ids`, `metadata` |
| `basic/run-input.mdx` | `run_input`, `client::RunRequest`, `server::input`, `server::helpers::PartFilter` |
| `basic/capabilities.mdx` | `capability`, `client::capabilities` |
| `basic/metadata.mdx` | `metadata`, `json`, `pipeline::reduce` |
| `basic/processing.mdx` | `pipeline::{compat, enforce, chunks, verify}`, `client::middleware` |
| `basic/versioning.mdx` | `version`, `pipeline::compat::EraShims`, `server::input` |
| `patterns/snapshots.mdx` | `patch`, `state`, `pipeline::reduce`, `server::state` |
| `patterns/streaming.mdx` | `pipeline::chunks`, `pipeline::verify`, `server::emit` |
| `patterns/interrupt-resume.mdx` | `interrupt`, `client::Thread`, `server::helpers::InterruptStore` |
| `transports/index.mdx` | `client::transport::Transport`, `server::http` |
| `transports/http-sse.mdx` | `client::transport::{HttpTransport, sse}`, `server::encode::sse` |
| `transports/http-protobuf.mdx` | `proto`, `wire`, `server::encode::proto` |
| `events/lifecycle.mdx` | `lifecycle`, `usage`, `pipeline::verify`, `server::Runner` |
| `events/text-messages.mdx` | `text` |
| `events/tool-calls.mdx` | `tool`, `content`, `client::Thread` |
| `events/state.mdx` | `state` |
| `events/activity.mdx` | `activity`, `message::ActivityMessage` |
| `events/reasoning.mdx` | `reasoning` |
| `events/subagents.mdx` | `subagent`, `server::emit::SubagentScope` |
| `events/passthrough.mdx` | `passthrough` |
| `schema-files.mdx` | `pipeline::enforce::validate_strict`, `event::Event::parse_strict` |

## 13. Differences from the 0.2 API, and why

The 0.2 public API (the client facade, the server crate root, core errors,
diagnostics and capabilities) is the starting point. What changed:

| 0.2 | Now | Why |
|---|---|---|
| Six crates (`ag-ui-core`, `-client`, `-server`, `-proto`, `-codegen`, `-test-support`) | One crate with features, plus an alias, a test-support crate and an xtask | One version, one error type, one doc site. Weight is controlled by features rather than crate boundaries. `ag-ui-server` was another author's crate name. |
| MSRV 1.88, edition 2024 | MSRV 1.75, edition 2021 | Owner requirement. Nothing needed past 1.75 once let-chains are avoided. |
| `HttpAgent` / `HttpAgentBuilder` | `AgentClient::http(url) -> ClientBuilder<HttpTransportBuilder>` | In one crate, "Agent" is the server trait; the client is named for its role. Transport and client options share one builder. |
| `RunUpdate { event, state: Arc<Value>, messages: Arc<Vec<Message>>, *_changed }` | `RunUpdate { event, changes }`; history and state borrowed via `RunHandle::messages()/state()` and `RunView` | Arc snapshots force copy-on-write. A consumer that keeps the last update alive makes every later event clone the whole history: O(n) per event. Borrowing is O(1) with zero allocation. |
| Middleware on `serde_json::Value` frames | Middleware on `Frame` (raw bytes or a parsed value) | With middleware installed, the innermost stage parses each frame once (the compatibility boundary needs the value); a pass-through or peeking middleware adds no further parse. With no middleware, the pipeline parses once, straight into typed events. |
| `Transport` yields `Value` frames; request carries `Value` body | `Transport` yields `Frame`s; `TransportRequest.body` is a pre-serialized `wire::Chunk` | No transport parses JSON it doesn't need to, and the input is serialized exactly once. |
| Pipeline internals hidden in the client (`event_handler`, `protocol_state`, `chunk`) | Public sans-IO `pipeline::{Processor, compat, enforce, chunks, verify, reduce}` | The same verdicts serve the client, the server's verifier, corpus tests and custom hosts. They are documented building blocks, not facade internals. |
| `compat::Subscriber` wrapping a 40-hook `AgentSubscriber` trait (TypeScript-shaped, `async-trait`) | `client::subscriber::Subscriber`: closures keyed by `EventType`, returning `Flow`, with `Mutation` | The hook-per-event trait was a TS design port (provenance) and a semver liability: each new event is a new hook. A closure registry is ours, and new events cost nothing. |
| `ag_ui_core::types::{…}`, generic `Event<S>`/`RunAgentInput<S, P>` | Area modules; non-generic types; `Event` structs without the `Event` suffix | Module path names the spec page. Generics leaked into every signature; typed views remain (`RunReport::state_as`, `RunRequest::state`, `server::Agent::State`). |
| `String` ids | `Arc<str>` newtypes | The verifier, chunk lanes and reducer clone ids per event. A clone is now a refcount bump. |
| `serde_json::Map` for metadata | `json::JsonObject` newtype; `Metadata = JsonObject` | Keeps the map type out of the API and gives the merge rule (META-06/07) a home. |
| Client `Error` plus separate `ProtoError`, `InputRejection`, capability `Rejected` | One `agui_sdk::Error` with codes; `PatchError` (non-fatal) and the §5 verdict types stay separate | Callers branch on one code space. |
| `HttpErrorBody` public type, `DEFAULT_RESPONSE_TIMEOUT` | `Error::http_status` + bounded message; connect, response-header (`response_timeout`), idle and total timeouts, with defaults documented on the setters rather than exported as constants | Smaller surface: no public error-body type or timeout constants. |
| Server: `Agent::run(&self, &mut RunContext)`, sync `Emit::emit`, `ready().await` for backpressure; emit handles and runner adapted from another author's PR | `Agent::run(&self, RunContext)` (owned); `async Emitter::emit` with backpressure built in; `start_*` item handles; verifier refusal at the call site | A new design (provenance: the 0.2 server shapes derived from #2778). Owned context avoids borrow gymnastics across `.await`; async emit makes backpressure impossible to forget. |
| Server requires tokio | `server` is runtime-agnostic: the run is driven inside `RunStream::poll_next`; tokio only under `axum` | Usable under any executor. |
| `EventEncoder` trait objects | `server::encode::Encoder` enum | Direct dispatch per event. |
| No framework-independent server HTTP layer | `server::http::handle` | Any framework can host the agent. axum is ~50 lines over it. |
| No replay support | `Runner::run_with_replay` + `ReplayedRun` | LIFE-43/46, RUNIN-31, SSE-11. |
| prost-generated proto types | Hand codec over a generated field table; `encode_json` for byte parity | Zero dependencies, JS key order for `Struct`. |
| `RunReport { messages: Arc<Vec<Message>>, new_messages: Vec<Message> }` | `RunReport { messages: Vec<Message>, new_message_ids }` + `new_messages()` | History is moved out of the reducer, not shared and duplicated; ids survive `MESSAGES_SNAPSHOT` reordering. |
| reqwest-based HTTP client, `reqwest-client` escape hatch | hyper 1 + hyper-util; proxy, root CA and mTLS as builder methods over PEM/DER bytes | D1: 47 vs 93 dependencies; no third-party type in the API. |
| axum `Router`/`MethodRouter` helpers | `AguiRequest: FromRequest`, `AguiResponse: IntoResponse` over axum-core | D2: 23 vs 41 dependencies; the application owns routing. |

## 14. Open questions and their resolutions

1. **PROC-17 (closed-set unknown values).** Resolved by D3: the spec wins.
   The Enforcer strips with a warning and is never fatal; the upstream
   fixture `unknown-enum-value-role-fatal` (which asserts the TypeScript
   reference's admitted gap) is overridden in the Rust lane as `intentional`
   (`tests/corpus/rust-lane-overrides.toml`). Raise the fixture upstream.
2. **Protobuf byte parity for typed open objects (PB-12).** Resolved by D4:
   the opt-in `preserve-order` feature. Without it the JSON-text path keeps
   parity and the typed path is conformant but not byte-identical.
3. **HTTP/2.** Not enabled (hyper `http1` only); add an `http2` feature when
   a user needs it.
4. **Draft 1.1** (`encryptedValueType`, `TOOL_CALL_RESULT.error`). Additive
   fields, added when 1.1 freezes; `#[non_exhaustive]` makes that a minor
   release.

## 15. Documentation conventions

- Every main entry point (`AgentClient::run`, `Thread`, `server::Agent`,
  `Runner`, the encoders, `pipeline::Processor`, `patch::apply`) carries a
  doctest, and the README's quick start is compiled as a doctest too
  (`ReadmeDoctests` in `src/lib.rs`, under `cfg(doctest)`).
- Docs build warning-free with `--all-features` (as on docs.rs), with
  `--no-default-features` and with each feature alone. An always-on item
  that links to a feature-gated one carries a
  `#[cfg_attr(not(feature = "…"), doc = "[link]: https://docs.rs/…")]`
  fallback, so the link resolves to docs.rs when the feature is off.
- `#[non_exhaustive]` result types that tests may need (`RunReport`,
  `RunInfo`, `RunView`, `Ingested`, `ResumeCheck`) have public
  constructors; `server::input::admit_input` builds an `AdmittedInput`.

## 16. Review resolution

Disposition of every finding in the independent API review of 088ef00
(44 findings). *Applied* means the signature change is in the API;
*partly* and *not applied* give the reason.

| # | Sev. | Finding | Disposition |
|---|---|---|---|
| 1 | blocker | `PatchOperation` `value: null` lost; `flatten` | Applied: hand-written `Serialize`/`Deserialize`, "present" rule (`Some(Null)` round-trips), no flatten. `json::deserialize_present` for other such members (none today). |
| 2 | blocker | Verifiers cannot see the input | Applied: `ProducerVerifier::for_input`, `ConsumerVerifier::expecting` + `with_known_messages`, `Processor::expecting`; new `ProducerRule::DuplicateMessageId`. |
| 3 | blocker | No server-side downgrade; `bool` cannot name an era | Applied: `EraShims::downgrade_event`, `EraShims::era`; `RunnerConfig.legacy_peer: Option<LegacyEra>` replaces `downgrade_legacy_peers`. |
| 4 | blocker | Fast path skips the boundary | Applied: `Processor` owns `Boundary` and `Option<EraShims>`; `push_value` documented as post-boundary. |
| 5 | major | Types admit wire-invalid values | Applied: `skip_serializing_if = "json::is_absent"` on every optional open-JSON member; setters normalise `Null`; `ProducerRule::InvalidValue` (pointers, ±2^53 integers, nulls); `JsonPointer::new_unchecked` documented as re-validated. |
| 6 | major | TOOL-33 lost across restarts | Applied: `ThreadSnapshot` (serde), `Thread::snapshot`/`restore`; `with_history` derives pending calls; `Reducer::restore`. |
| 7 | major | Agent cannot choose `RUN_ERROR` | Applied: `RunOutcome::failed(RunError)`, `Error::public`/`is_public`. |
| 8 | minor | `new_messages_start` wrong after snapshot | Applied: `RunReport.new_message_ids`, `new_messages()` iterator, `Reducer::run_message_ids`. |
| 9 | minor | CAP-03 setter is a claim | Applied: `CapabilitiesBuilder::served_transports` removed; `capabilities_response` derives the flags. |
| 10 | minor | Absent vs `[]` lost on input | Applied: `tools`/`context`/`resume` are `Option<Vec<_>>` with slice accessors. |
| 11 | minor | Derived `Deserialize` semantics unspecified | Applied as documentation (crate root, `event`): unchecked decoding of trusted stored data; `Processor` is the receive path. Not hand-implemented: the shape-table seed is that path already. |
| 12 | major | Cancellation cannot wake; two types | Applied: one `cancel::CancelToken` (atomic + waker list) for client and server; `RunCanceller` removed; `RunRequest::cancel_token`, `RunHandle::cancel_token`, `TransportRequest.cancel`. |
| 13 | major | Iterating needs futures-util | Applied: inherent `next()`/`poll_next()` on `RunHandle`, `FrameStream`, `RunStream`, `ResponseBody`; `Stream` impls only under feature `stream`. |
| 14 | major | Observers cannot tell runs apart | Applied: `RunInfo`; `on_diagnostic(.., run)`, `on_finish(.., run)`; `RunView::{run, thread_id, run_id, pending_tool_calls}`. |
| 15 | major | `?` fails on application errors | Applied: `Agent::run -> Result<RunOutcome, BoxError>`; `BoxError` at crate root and `server`. |
| 16 | major | No request-scoped data | Applied: `server::Extensions`; `HttpRequest.extensions`/`with_extensions`, `AguiRequest::with_extensions`, `RunContext::extensions`, `Runner::run(.., extensions)`. |
| 17 | major | `TransportRequest` too thin | Applied: `thread_id`, `run_id`, `headers: Vec<Header>`, `TransportRequest::new`; `RunRequest::header`/`sensitive_header`. |
| 18 | major | Thread runs not spawnable | Applied: `Thread::run/send/resume -> RunHandle` (`'static + Send`), `Thread::absorb`, `run_and_absorb`; `ThreadRun` removed. A second `run` before `absorb` is refused. |
| 19 | major | `validated_args` overstates | Applied: `parse_args`, `check_args_subset -> SchemaCheck { unevaluated }`, tool captured at `approve` (`tool()`, `is_advertised()` take no slice). |
| 20 | minor | Inconsistent `build`; stringly pin | Applied: generic `ClientBuilder::build -> Result`; `peer_version(PeerPin)`. |
| 21 | minor | Middleware hard to write | Applied: `map_frames`; blanket `Middleware` for closures; `FrameStream::{new, from_poll_fn, from_frames, empty}` and `FrameSource`. |
| 22 | minor | `Frame::event_type` cannot borrow escapes | Applied: `event_type() -> Option<EventType>`, `raw_type() -> Option<Cow<str>>`. |
| 23 | minor | `Default` decoders reject everything | Applied: `Default` removed from `SseDecoder`/`FrameDecoder`; `sse::DEFAULT_MAX_EVENT_BYTES` added. |
| 24 | minor | `State: Default` vs RUNIN-24; late failure | Applied: `Default` bound dropped (absent reads as `{}`); `Runner::run`/`run_with_replay` return `Result<RunStream>` so a misfit state is a 400 before the stream. Chose a fallible `run` over a separate `prepare`/`PreparedRun` pair: one entry point, same guarantee. |
| 25 | minor | `record_usage` on `&mut RunContext` | Applied: `Emitter::record_usage(&self)` on shared run state; removed from `RunContext`. |
| 26 | minor | Seal the boxed twins | Applied: `DynTransport`, `DynAgent` sealed. |
| 27 | minor | Module tree | Applied: `pipeline::middleware` → `client::middleware`; `client::compat` → `client::subscriber` (`ClientBuilder::compat_subscriber` → `subscriber`); `prelude` added; `server::verify` dropped (use `pipeline::verify`). |
| 28 | minor | Hidden global side effect in `MetadataBuilder` | Applied: `try_insert -> Result<Self, Diagnostic>`; `insert` skips the reserved key with no side effect. |
| 29 | minor | Capability fetch bypasses transport settings | Applied: `HttpTransport::fetch_capabilities(&CapabilitiesRequest)`; `CapabilitiesRequest::fetch` removed. |
| 30 | minor | Unbounded concrete `InterruptStore` | Applied, with one change: the trait is async (RPITIT) so a database can back it, and `check` returns `ResumeCheck.unrecognised` instead of taking a `Reporter`. `MemoryInterruptStore::new(max_threads, ttl)`. |
| 31 | major | Third-party types in signatures | **Disagreed with the proposed fix** (re-export `bytes`/`futures-core` as public deps): the rule is no third-party type but `serde_json::Value`. Instead: own `wire::Chunk` (backed privately by `bytes`), own stream types with inherent `next`, `futures_core::Stream` only behind `stream`; `axum::Router` removed per D2. Policy in §9. |
| 32 | minor | Missing constructors | Applied: `TransportRequest::new`, `RunUpdate::new`, `PendingToolCall::new`, `ProducerViolation::new`, `const ProtocolVersion::new`. |
| 33 | major | `Vec<u8>` per event in `ResponseBody` | Applied: `ResponseBody` yields coalesced `Chunk`s (own type, not `Bytes`); `keepalive() -> Option<Chunk>`. |
| 34 | major | Reducer clones payloads §8 omits | Applied beyond the review's minimum (a deliberate choice): `Reducer::apply` takes `Event` by value; `Processed.event` and `RunUpdate.event` are `Arc<Event>`; payloads are taken without cloning when the update is dropped, copy-on-write otherwise. §8 rows rewritten. |
| 35 | major | Outgoing input `Value` round trip | Applied: `check_outgoing_input(&RunAgentInput)` and `enforce_outgoing_value(&mut Value)` replace `enforce_outgoing_input`. |
| 36 | minor | `Event` 384 B | Applied: `RunStarted.input: Option<Box<RunAgentInput>>`; `Event` now 200 B (320–336 B with `preserve_order`); compile-time cap 384 B. |
| 37 | minor | `Error` 80 B | Applied: `Error { inner: Box<ErrorInner> }`, 8 B, asserted. |
| 38 | minor | Two allocations per id | Applied: hand-written `Deserialize` for ids (`visit_str` → `Arc::from`). Interning against the verifier's open set is left to the pipeline implementation (no signature impact). |
| 39 | minor | `Diagnostic` allocations; no run id | Applied: `event_type: Option<EventType>`, `unknown_type: Option<Box<str>>`, `run_id: Option<RunId>`. `pointer` stays `String`: most diagnostics carry a computed pointer, so the allocation is needed anyway. |
| 40 | minor | MSRV traps | Applied as documentation (§10) and README; `clippy.toml` already sets `msrv`. |
| 41 | blocker | D1 | Applied: hyper stack, `reqwest-client` removed; `proxy`, `proxy_from_env`, `tls_roots(TlsRoots)`, `add_root_certificate_der`, `add_root_certificates_pem`, `client_identity_pem`, `tls_backend(TlsBackend)`; reserved headers extended; redirect header stripping is ours; `https://` without TLS fails `build`. Default features now empty. Docs, CI matrix, alias crate updated. |
| 42 | blocker | D2 | Applied: `server::axum::{AguiRequest, AguiResponse, respond}` over axum-core 0.5; no `Router`. |
| 43 | major | D3 | Applied: SPEC-MAP flag table and PROC-17 row, §14, enforce docs (required position → drop the container), `tests/corpus/rust-lane-overrides.toml` entry. |
| 44 | major | D4 | Applied: `preserve-order` feature (crate and alias), docs in `json`, `proto`, SPEC-MAP PB-12, §9, §14; CI matrix row. A typed-path corpus variant is future work. |
