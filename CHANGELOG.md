# Changelog

All notable changes to this project are documented in this file. Both crates,
`agui-sdk` and its alias `ag-ui-sdk`, are versioned together and share it.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.0] - 2026-11-11

The first release. Implements **AG-UI protocol 1.0**. Both crates carry
version 0.9.0.

### Added

- AG-UI 1.0 protocol types: serde types for every schema entry (events,
  messages, tools, run input, capabilities and state), `#[non_exhaustive]`
  with constructors and `with_*` setters.
- The sans-IO processing pipeline (`pipeline::Processor`): compatibility
  boundary, enforcement of the spec's unknown-field and unknown-value rules,
  chunk expansion, stream verification and the reducer. Bounded, atomic JSON
  Patch (`patch`).
- Conformant client (feature `client`): `AgentClient`, `Thread`, HTTP
  transport over hyper 1 (HTTPS through `rustls` or `native-tls`; `client`
  alone is plain `http://`), middleware and observers. Run lifecycle
  enforcement, sequential runs, `STATE_SNAPSHOT` / `STATE_DELTA` and
  `MESSAGES_SNAPSHOT` handling.
- Conformant, runtime-agnostic server (feature `server`): `Agent`, `Runner`,
  `RunContext`, a producer verifier, SSE, protobuf and NDJSON encoders, the
  framework-free `server::http` layer, `InterruptStore` (implementable outside
  the crate through `ResumeCheck::new`/`classify`), and axum 0.8 integration
  (feature `axum`).
- Protobuf wire (feature `proto`), `futures_core::Stream` impls (feature
  `stream`), diagnostics to `tracing` or `log`.
- `ag-ui-sdk`, an alias crate re-exporting `agui-sdk` with the same features,
  pinned to the same version.
- Conformance suite run against the upstream specification on every qa run,
  with every normative requirement traced to tests (`docs/SPEC-MAP.md`).
- Fuzz targets for every parser, and a perf gate for allocation budgets and
  linearity.
- Stream limits and reconciliation in the pipeline:
  - `Limits::max_history_bytes` (default 128 MiB): the most the held message
    history may grow by over one stream, as estimated resident bytes.
    Exceeding it ends the stream with `processing.limit`. The estimate is net
    of overwritten metadata values and of values activity patches replace or
    remove, so updating in place is not growth; a message a snapshot drops or
    replaces is credited with exactly what it was charged.
  - `MESSAGES_SNAPSHOT` reconciles in place, so a snapshot that changes little
    does not cost O(held history). Retained activity contents keep counting
    against the shared patch node ceiling across snapshots.
  - A reconcile work bound on `MESSAGES_SNAPSHOT`: a snapshot whose drops
    would walk more held messages than the reconcile work bank holds is not
    applied. It is reported as `snapshot.work_bound`, and the `Processor`
    ends the stream with `processing.limit`. The bank holds about one million
    steps plus eight per held message, and gains eight per message a snapshot
    restates or the stream pushes. The bound is separate from
    `max_history_bytes`: it still applies when that limit is `usize::MAX`,
    and it is not configurable.
  - JSON Patch node accounting charges text: a `PatchBudget` counts node
    units, one per value plus one per 64 bytes of string or object-key text,
    and a `copy` is refused before its clone is allocated. Accumulated text
    reaches the ceiling (`processing.limit`) at most 22,680,000 bytes (about
    22 MiB) of text at the default 180,000-unit ceiling: each unit holds at
    most 126 bytes, a 63-byte key with a 63-byte string value. `PatchBudget::nodes()` and
    `PatchLimits::{min_nodes, max_nodes}` are in node units, and a
    document's per-patch walk and growth allowances scale with its text as
    well as its node count.
- Diagnostics `stream.missing_target` (a content, argument or end event, or
  a `TOOL_CALL_CHUNK` that would open a call without a name, names a message
  or call the reducer does not hold) and `stream.conflicting_reopen` (a
  reopening `TOOL_CALL_START` names a different parent: the call stays where
  it is, as in the TypeScript reference, and the latest name wins).
- `PartFilter::filter_messages`: filters a run input's messages in place and
  roots each skipped-part warning in the run input (`/messages/M/content/N`,
  `Scope::RunInput`). `filter_message` keeps message-relative pointers
  (`/content/N`).
- Wire handling matches the reference parsers: a repeated JSON member
  resolves as `JSON.parse` does (the last occurrence wins) on every path,
  including run input a server receives (`validate_strict` still rejects one
  at a schema position); the protobuf decoder reads an enum varint wider than
  32 bits as its low 32 bits, as protobuf `int32` parsing does; a
  `TOOL_CALL_RESULT` whose `messageId` names a held non-tool message is
  appended as a tool message rather than replacing it.
- A tool call with no parent message is carried by an assistant message the
  reducer mints under the call's id. When the producer later uses that id for
  a message of its own (a text or reasoning message, a tool result, an
  activity), the carrier is renamed `<id>~<n>` and keeps its calls, and the
  producer's message is a new message under its id, so it is never merged
  into the carrier (`docs/DECISIONS.md` D29). `run_message_ids` names
  exactly the run's messages still held, one entry per message, after any
  `MESSAGES_SNAPSHOT`.
- JSON Patch operations keep their undefined members, `from` included: on
  an op other than `move` or `copy`, a string `from` stays in
  `PatchOperation::from` and any other type in `extensions`, and the event
  is delivered whole. The protobuf encoder carries a string `from` and drops
  a non-string one, as it drops every extension member
  (`docs/DECISIONS.md` D23).
- The SSE decoder dispatches an event whose `data` is empty, as WHATWG
  specifies, and the pipeline refuses it as a frame that maps to no event,
  as it does `data:\ndata:\n\n`. Keep-alive comment lines are ignored
  (`docs/DECISIONS.md` D24).
- `ProducerVerifier::for_input` lets each replayed run before the requested
  one declare its own `protocolVersion`; `ProducerVerifier::new` holds every
  run on the stream to the version the first one declared.
- `patch::diff` of equal documents is empty, and a pure-removal delta stays a
  delta, even when the baseline is over `max_nodes`; a new member's key is
  charged, and the root `replace` fallback is documented.
- The runner closes what an agent left open (DECISIONS.md D12), and
  `SubagentScope::finish` with a `suspended` outcome finishes the scope's
  still-active descendants as `suspended`.
- `#[must_use]` on pure accessors (id and code `as_str`, `RunContext`
  accessors, `Chunk::len` and others) and on the server emitter's open-item
  handles (`TextStream`, `ToolCallStream`, `ReasoningSpan`, `ReasoningStream`,
  `StepSpan`, `SubagentScope`): a dropped handle leaves its item open until
  the run's closers close it, so write `let _ =` when that is intended.
- Panic containment. On the client, a panicking diagnostic sink, observer,
  subscriber, middleware or custom transport fails the run instead of
  unwinding into the caller; a transport that panics while opening or
  reading fails it with `transport.stream_read` ("the transport panicked"),
  with or without middleware (DECISIONS.md D17). On the server, a panic in an
  agent destructor run when the client disconnects mid-run does not escape
  the stream's drop, and the run is closed before those destructors run, so
  an emit waiting for queue space fails instead of blocking them.
- MSRV: Rust 1.75 for every feature except `native-tls`, which needs 1.80
  (its advisory-clean `openssl`).

### Security

- The client's default limits keep what one stream makes it hold near
  252 MiB in the worst case (64-bit; `docs/DECISIONS.md` D14): about 124 MiB
  of state and patched activity content plus 128 MiB of history and id
  bookkeeping growth. The figure is stated on `Limits`, `Limits::default` and
  `PatchLimits::max_nodes`, with what raising each limit for a large state
  costs:
  - `max_history_bytes` (default 128 MiB) is charged in estimated resident
    bytes: the `serde_json` tree's slots, map nodes and string heaps, plus
    about 280 bytes per held message and call.
  - The same budget covers the ids the pipeline keeps to check and track the
    stream: open steps, reasoning spans, subagent invocations, the run's
    item and tool-call tables, the message ids the thread has used (kept
    after a `MESSAGES_SNAPSHOT` drops the message), and the copies of the
    calls a run ends with pending, charged until the next run starts, and
    the retired `THINKING_*` spans the compatibility boundary tracks (one
    entry per open id, closed by `THINKING_END` or a 1.0 `REASONING_END`;
    `docs/DECISIONS.md` D28, with or without middleware). Chunk streams
    keep fingerprints of the opener fields they check later chunks against,
    not copies, keyed at random per process so a peer cannot craft a
    conflicting value that compares equal. A
    stream that floods any of them ends with `processing.limit` ("the
    message history and the stream's id bookkeeping grew by ..."); one whose
    pending calls cross the limit ends on its `RUN_FINISHED`
    (`docs/DECISIONS.md` D25).
  - Within one activity patch, a value written at a member of an object the
    content already held and then removed, replaced or moved over again is
    credited back, so long-lived activities updated with multi-operation
    patches hold their charge steady (values up to about 64 KB;
    `docs/DECISIONS.md` D14).
  - `PatchLimits::max_nodes` defaults to 180,000 node units, about 124 MiB
    of state and patched activity content in the worst shape.
  - `Limits::max_container_entries` defaults to 150,000, so one
    `STATE_SNAPSHOT` installs at most about 103 MiB in the worst shape. It
    also bounds server input admission, `validate_*` and the protobuf
    decode budget.
- `proto::decode_event` and `decode_event_json` state their amplification:
  up to about 350 times the frame in memory, and about 10 times as JSON text,
  and point untrusted input at `Processor::push_json`, which enforces the
  limits.
- The `native-tls` backend offers TLS 1.2 at least, as the `rustls` backend
  does, whatever the platform allows (native-tls defaults to TLS 1.0;
  `docs/DECISIONS.md` D19).
- The axum extractor `AguiRequest` reads at most 16 MiB of a body, even under
  `DefaultBodyLimit::disable()`, and answers a `Content-Length` over 16 MiB
  with `413` before reading it. A body over 16 MiB is therefore refused even
  when axum's limit and `InputPolicy::max_body_bytes` were both raised; read
  such bodies yourself and pass them to `AguiRequest::new`
  (`docs/DECISIONS.md` D20).
- The server's error body attached to a `transport.http_status` error (its
  `Error::source`) displays with control characters escaped, as `Error`'s
  own `Display` does, so logging the error chain cannot write forged lines or
  terminal escapes. Pretty-printed JSON bodies read with `\n` escapes
  (`docs/DECISIONS.md` D26).
- `MemoryInterruptStore` also bounds the thread ids it keeps to 1 KiB per
  allowed thread in total (about 10 MiB by default), evicting the least
  recently recorded threads first, so a client cannot pin multi-megabyte
  thread ids for the time to live (`docs/DECISIONS.md` D27).

[Unreleased]: https://github.com/MattJackson/agui-sdk/compare/v0.9.0...HEAD
[0.9.0]: https://github.com/MattJackson/agui-sdk/releases/tag/v0.9.0
