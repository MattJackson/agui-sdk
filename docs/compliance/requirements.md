# AG-UI 1.0 normative-requirements ledger

Sources: `docs/spec/1.0/**/*.mdx` (excluding the generated `schema.mdx`; `schema-files.mdx` added in rev 2, §30), `docs/spec/draft/changelog.mdx`, and a diff of `spec/draft/schema.json` vs `spec/1.0/schema.json`.
All `src` paths are relative to `docs/spec/1.0/` in the upstream spec repository (ag-ui-protocol/ag-ui, at the commit pinned in `spec.lock`) and cite `file:line` (line numbers are the physical `.mdx` lines, frontmatter included).
Fixtures are the `name` of files in `spec/1.0/conformance/streams/` in the same repository (68 fixtures plus `MANIFEST.txt`; `-` means none exercises the rule). Draft-only fixtures (`spec/draft/conformance/streams/`) are cited only in the §26 DRAFT table, prefixed `draft:`.

Legend. `kw`: MUST, MUST NOT, SHOULD, SHOULD NOT, MAY (RFC 2119 words as written in capitals); REQUIRED / OPTIONAL (BCP 14 words used for fields); `implicit` = behavioural rule stated without a capitalised keyword. `party`: producer / consumer / both / transport. A leading "(restates X)" marks a rule repeated on another page; it is still counted once per occurrence. "fixture (partial)" means the fixture exercises only one facet.

Fixture-matching note: fixtures all live in one directory and are consumer-side (the harness feeds a stream to a client and asserts outcome, events, warnings, messages). Producer-side MUST NOTs are therefore only covered where the fixture shows the consumer rejecting the violation; those are marked as the matching fixture anyway.

Revision 2 (2026-11-10, classification): counts regenerated from the table; keyword, source-line and fixture errors corrected; 15 rows appended (IDs appended, none renumbered). Every change is listed in `compliance/README.md` §Ledger corrections. Machine-readable classification: `compliance/compliance.toml`.

Conformance model (index.mdx:57-66): an implementation conforms when it satisfies every MUST / MUST NOT for the roles it plays; SHOULD departures need a reason, not a violation; conformance is judged per stream.

## 1. index.mdx (Specification)

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| IDX-01 | implicit | both | Schema is authoritative for structure; the spec pages are authoritative for behaviour; on structural conflict schema wins | index.mdx:33-43 | - |
| IDX-02 | implicit | both | A participant that is both producer and consumer is bound by both rule sets, each direction separately | index.mdx:54-55 | - |
| IDX-03 | implicit | both | Conformance = every MUST/MUST NOT for the roles played; SHOULD departure needs a reason | index.mdx:59-62 | - |
| IDX-04 | implicit | both | Conformance is judged per stream; one malformed run makes the producer non-conformant | index.mdx:64-65 | - |
| IDX-05 | SHOULD | consumer | Application obtains explicit user consent before executing side-effectful tool calls | index.mdx:98-100 | - |
| IDX-06 | MUST NOT | consumer | Represent an action as user-approved when it was not | index.mdx:100-101 | - |
| IDX-07 | MUST | consumer | Applications validate what they act on (tool args/results, state, passthrough payloads) | index.mdx:104-106 | - |
| IDX-08 | MUST NOT | consumer | Render streamed content as executable markup | index.mdx:105-106 | - |
| IDX-09 | SHOULD NOT | producer | Put secrets in state or messages (they round-trip) | index.mdx:108-109 | - |
| IDX-10 | SHOULD | consumer | Treat reasoning and encrypted artefacts with the same confidentiality as the conversation | index.mdx:109-110 | - |
| IDX-11 | SHOULD | both | Build consent flows, validate at every trust boundary, isolate rendering from execution, log for audit | index.mdx:114-117 | - |
| IDX-12 | implicit | both | Every rule applies equally to TS, Python and .NET SDKs; community bindings bound when they claim conformance | index.mdx:126-129 | - |

## 2. architecture.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| ARCH-01 | implicit | consumer | Client pipeline order: compatibility translation, middleware, enforcement, chunk expansion, verification, then application | architecture.mdx:41-44 | - |
| ARCH-02 | implicit | consumer | Middleware sees every event before enforcement strips anything | architecture.mdx:55-57 | era-0-0-45-thinking-translated |
| ARCH-03 | implicit | both | A consumer opens an exchange with one RunAgentInput; producer answers with events bracketed by run lifecycle; next run reuses threadId | architecture.mdx:62-66,78 | two-sequential-runs-accumulate (partial) |
| ARCH-04 | MUST | producer | Emit only what the schema defines (strict writer) | architecture.mdx:94-95 | - |
| ARCH-05 | MUST | consumer | (restates PROC) Survive unrecognised material, stripping with a warning rather than failing | architecture.mdx:95-96 | unknown-property-stripped |
| ARCH-06 | implicit | transport | Semantics never vary by wire; cross-implementation parity pinned by a shared byte corpus | architecture.mdx:106-108 | - |

## 3. basic/index.mdx (event model)

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| BASIC-01 | MUST | both | All implementations support the event model and the event patterns | basic/index.mdx:26-27 | - |
| BASIC-02 | REQUIRED | producer | Every event carries `type`, one of the 31 `EventType` values (schema has 31) | basic/index.mdx:35-36 | - |
| BASIC-03 | OPTIONAL | producer | `timestamp`; by convention ms since Unix epoch | basic/index.mdx:37-39 | - |
| BASIC-04 | MUST NOT | consumer | Use `timestamp` to order events; arrival order is the protocol's order | basic/index.mdx:37-39 | - |
| BASIC-05 | OPTIONAL | producer | `rawEvent` carries the provider-native event verbatim | basic/index.mdx:40-41 | - |
| BASIC-06 | MUST NOT | consumer | Derive protocol behaviour from `rawEvent` | basic/index.mdx:41-42 | - |
| BASIC-07 | OPTIONAL | producer | `metadata` on events; `subagentRunId` on Attributable events | basic/index.mdx:43-47 | - |
| BASIC-08 | implicit | producer | Run-scoped events (RUN_STARTED, RUN_FINISHED, RUN_ERROR, MESSAGES_SNAPSHOT) carry no attribution | basic/index.mdx:47-49 | - |
| BASIC-09 | MUST | producer | Omit an optional field that has no value rather than send `null` (incl. rawEvent, result, forwardedProps) | basic/index.mdx:62-66 | - |
| BASIC-10 | implicit | both | A required JSON payload may be `null` (CUSTOM.value, STATE_SNAPSHOT.snapshot) | basic/index.mdx:66-67 | state-null-snapshot-applies |
| BASIC-11 | MUST | consumer | Preserve a `null` value under an open-by-key object (metadata value, inside state) | basic/index.mdx:76-78 | open-metadata-preserved |
| BASIC-12 | implicit | consumer | Shipped tolerance of `null` in optional positions is a compatibility shim, not this rule | basic/index.mdx:80-82 | - |
| BASIC-13 | implicit | producer | `threadId` minted by the application, stable across runs | basic/index.mdx:86-87 | - |
| BASIC-14 | MUST NOT | producer | Reuse a `runId` for another run on the same thread | basic/index.mdx:88-89 | - |
| BASIC-15 | MUST | producer | `messageId` unique within its thread (crosses run boundaries) | basic/index.mdx:90-92 | - |
| BASIC-16 | MAY | producer | A snapshot in a later run restates a message by its id | basic/index.mdx:90-91 | messages-snapshot-reconciles-by-id |
| BASIC-17 | implicit | both | `toolCallId` ties a call to its result and any interrupt concerning it | basic/index.mdx:93-94 | - |
| BASIC-18 | implicit | producer | `subagentRunId` is an opaque per-invocation handle; two invocations carry different values | basic/index.mdx:95-97 | - |
| BASIC-19 | MUST NOT | consumer | Parse structure out of identifiers (all opaque strings) | basic/index.mdx:99-100 | - |

## 4. basic/run-input.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| RUNIN-01 | implicit | consumer | Exactly one message flows consumer to producer, sent once to open each exchange | basic/run-input.mdx:6-8 | - |
| RUNIN-02 | MAY | producer | Echo the input back on `RUN_STARTED.input` | basic/run-input.mdx:13-15 | - |
| RUNIN-03 | REQUIRED | consumer | `threadId` and `runId` on the input | basic/run-input.mdx:19-20 | - |
| RUNIN-04 | implicit | producer | Every run on the stream carries the input's `threadId` on RUN_STARTED and RUN_FINISHED | basic/run-input.mdx:20-22 | - |
| RUNIN-05 | MUST | producer | A run's two boundary events agree on `runId` | basic/run-input.mdx:22-23 | - |
| RUNIN-06 | implicit | producer | The requested run echoes the input's `runId`; a replayed run carries its own | basic/run-input.mdx:23-24 | - |
| RUNIN-07 | OPTIONAL | producer | `parentRunId` names the run that spawned this one | basic/run-input.mdx:24-25 | - |
| RUNIN-08 | MUST | consumer | Declare its protocol version in `protocolVersion`, except toward a peer known to predate the field | basic/run-input.mdx:29-32 | current-peer-declares-protocol-version, era-pinned-peer-omits-protocol-version |
| RUNIN-09 | REQUIRED | consumer | `messages`: the conversation so far, in order | basic/run-input.mdx:38 | - |
| RUNIN-10 | MUST | producer | Treat `messages` as the complete history (no side channel) | basic/run-input.mdx:38-40 | - |
| RUNIN-11 | MUST NOT | producer | Fail the run because it cannot use a content part; skip it and continue | basic/run-input.mdx:49-52 | - |
| RUNIN-12 | MUST | consumer | Strip activity messages from `messages` before sending | basic/run-input.mdx:54-56 | - |
| RUNIN-13 | MUST NOT | both | Fetch, parse, or read a scheme out of a `file` source `value`; hand to the provider or do not use the part | basic/run-input.mdx:69-71 | tool-result-file-source-passes-through (partial) |
| RUNIN-14 | SHOULD | producer | When `provider` is present on a file source, use the lowercase vendor id of `TokenUsage.provider` (field OPTIONAL) | basic/run-input.mdx:72-76 | - |
| RUNIN-15 | MUST NOT | producer | Fail the run on an unresolvable/expired/foreign file handle; skip the part and continue | basic/run-input.mdx:77-80 | - |
| RUNIN-16 | SHOULD | producer | Say so when skipping an unresolvable handle | basic/run-input.mdx:80 | - |
| RUNIN-17 | implicit | producer | A producer that wants to carry a handle expiry does so in the part's `metadata` (no keyword in the text) | basic/run-input.mdx:80-82 | - |
| RUNIN-18 | implicit | both | Source arms are a closed set: a source `type` not recognised "fails validation rather than being carried through" (see ambiguity A1) | basic/run-input.mdx:83-86 | - |
| RUNIN-19 | OPTIONAL | consumer | `tools` (frontend tools: name, description, parameter schema) | basic/run-input.mdx:90-92 | - |
| RUNIN-20 | MUST | producer | Treat missing `tools` exactly as empty | basic/run-input.mdx:97-98 | - |
| RUNIN-21 | MAY | consumer | Omit an empty `tools` list rather than send `[]` | basic/run-input.mdx:98-99 | - |
| RUNIN-22 | SHOULD | producer | Make every `context` entry available to the model as grounding | basic/run-input.mdx:103-106 | - |
| RUNIN-23 | implicit | producer | Absent `context` = empty | basic/run-input.mdx:110 | - |
| RUNIN-24 | implicit | producer | Absent `state` means `{}`; the run starts from the input's `state` | basic/run-input.mdx:114-117 | - |
| RUNIN-25 | MUST | producer | A producer emitting deltas computes them against the input `state` until its own first snapshot | basic/run-input.mdx:117-118 | - |
| RUNIN-26 | implicit | consumer | `forwardedProps`: any JSON value except whole `null` (means omit); inner nulls preserved | basic/run-input.mdx:122-124 | - |
| RUNIN-27 | MUST NOT | both | Intermediaries alter `forwardedProps` | basic/run-input.mdx:124-125 | - |
| RUNIN-28 | OPTIONAL | consumer | `resume`: answers to the interrupts that ended the previous run | basic/run-input.mdx:129-131 | - |
| RUNIN-29 | implicit | producer | Reject unparseable or malformed input before RUN_STARTED, via the transport's error path (no RUN_ERROR; no stream exists) | basic/run-input.mdx:135-139 | - |
| RUNIN-30 | implicit | producer | Unrecognised input members are not malformed: strip with warning and run | basic/run-input.mdx:141-145 | - |
| RUNIN-31 | implicit | producer | The run the input requests is the exchange's last; a stream replaying history carries the earlier runs first, without further input | basic/run-input.mdx:8-10 | - |

## 5. basic/capabilities.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| CAP-01 | OPTIONAL | producer | Every capability group and every field in a group is optional | basic/capabilities.mdx:15,21-24 | - |
| CAP-02 | MUST NOT | consumer | Infer absence of a capability from absence of its declaration (omitted = undeclared) | basic/capabilities.mdx:26-30 | - |
| CAP-03 | SHOULD | producer | Declare only what the agent does; what is declared should hold | basic/capabilities.mdx:32-36 | - |
| CAP-04 | MUST NOT | consumer | Reject a stream or treat a run as failed because an event was not anticipated by a declaration or a declared capability went unexercised | basic/capabilities.mdx:38-43 | - |
| CAP-05 | implicit | both | `tools.items` (agent's own tools) never merges with `RunAgentInput.tools` | basic/capabilities.mdx:81-85 | - |
| CAP-06 | implicit | both | `multiAgent.subagents` lists definitions, not invocations; wire ids are `subagentRunId` | basic/capabilities.mdx:87-91 | - |
| CAP-07 | MUST NOT | consumer | Expect a standard event to carry structured-output schema or marker (`output.structuredOutput`) | basic/capabilities.mdx:67-72 | - |
| CAP-08 | MUST NOT | consumer | Expect a standard event to carry multimodal output (`multimodal.output`) | basic/capabilities.mdx:74-79 | - |
| CAP-09 | MAY | producer | Declare `transport.resumable` / `pushNotifications` for a transport of its own | basic/capabilities.mdx:97-98 | - |
| CAP-10 | MUST NOT | consumer | Expect either standard HTTP binding to honour resumable/pushNotifications | basic/capabilities.mdx:98-99 | - |
| CAP-11 | MUST | consumer | Preserve unrecognised members inside `identity.metadata` and `custom` | basic/capabilities.mdx:106-108 | - |
| CAP-12 | MUST | both | Any implementation exposing capabilities uses the schema's shape | basic/capabilities.mdx:121-122 | - |
| CAP-13 | MUST NOT | consumer | Reject a declaration for carrying unrecognised groups/fields | basic/capabilities.mdx:131-133 | - |
| CAP-14 | implicit | consumer | Whether unknown capability members are kept or dropped is implementation-defined (not pipeline-enforced) | basic/capabilities.mdx:133-136 | - |
| CAP-15 | MUST NOT | consumer | Use a declaration it cannot validate (known field with schema-rejected value) | basic/capabilities.mdx:136-138 | - |
| CAP-16 | MUST NOT | consumer | Let a rejected or absent declaration prevent a run; proceed as for an agent that declared nothing | basic/capabilities.mdx:140-142 | - |

## 6. basic/metadata.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| META-01 | implicit | both | `metadata` is open by key, any JSON value (incl. `null`, which is data) under a key | basic/metadata.mdx:14-15 | open-metadata-preserved |
| META-02 | MUST NOT | producer | The metadata object itself be `null` (absent or an object) | basic/metadata.mdx:15-16 | - |
| META-03 | implicit | consumer | Strip unknown metadata keys (processing model never strips them, any key) | basic/metadata.mdx:18-20 | open-metadata-preserved |
| META-04 | implicit | both | The `ag-ui` key is reserved for the protocol | basic/metadata.mdx:24-25 | - |
| META-05 | SHOULD | producer | Prefix invented keys with a vendor/application name | basic/metadata.mdx:25-26 | - |
| META-06 | MUST | consumer | Merge metadata of every event making up an item, key by key, last write wins | basic/metadata.mdx:32-34 | metadata-merges-last-write-wins |
| META-07 | MUST NOT | consumer | Recurse when merging; object/array values replaced wholesale | basic/metadata.mdx:36-38 | metadata-merges-last-write-wins |
| META-08 | implicit | consumer | TEXT_MESSAGE_START/CONTENT/END merge into the text message | basic/metadata.mdx:47 | metadata-merges-last-write-wins |
| META-09 | implicit | consumer | REASONING_MESSAGE_START/CONTENT/END merge into the reasoning message | basic/metadata.mdx:48 | - |
| META-10 | implicit | consumer | TOOL_CALL_START/ARGS/END merge into the tool call itself, not the owning assistant message | basic/metadata.mdx:49,55-61 | tool-call-metadata-lands-on-the-call |
| META-11 | implicit | consumer | TOOL_CALL_RESULT metadata merges into the tool message it creates | basic/metadata.mdx:50 | - |
| META-12 | implicit | consumer | ACTIVITY_SNAPSHOT/DELTA merge into the activity message; accumulated metadata survives a replacing snapshot | basic/metadata.mdx:51 | - |
| META-13 | implicit | consumer | `*_CHUNK` metadata merges into whatever its synthesized events merge into | basic/metadata.mdx:52 | - |
| META-14 | implicit | consumer | RUN_*, STEP_*, snapshots, RAW, CUSTOM etc. have no merge target | basic/metadata.mdx:53,63-64 | - |
| META-15 | MAY | consumer | Surface metadata of a no-target event (e.g. RUN_FINISHED) in own bookkeeping | basic/metadata.mdx:65-66 | - |
| META-16 | implicit | consumer | Stored `metadata` on messages/tool calls/tools/interrupts/resume entries is plain data; only listed events merge into it | basic/metadata.mdx:70-73 | - |
| META-17 | OPTIONAL | both | `metadata` is carried (optional) on events, messages, tool calls, tools, interrupts and resume entries | basic/metadata.mdx:6-7 | open-metadata-preserved (partial) |

## 7. basic/processing.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| PROC-01 | MUST NOT | consumer | Abort the run on an event type the implementation does not recognise | basic/processing.mdx:14-15 | unknown-event-dropped |
| PROC-02 | MUST NOT | consumer | Abort the run on an undescribed property of an event | basic/processing.mdx:16 | unknown-property-stripped |
| PROC-03 | MUST NOT | consumer | Abort the run on an unrecognised union member (new content part kind, later outcome) | basic/processing.mdx:17-18 | unknown-outcome-stripped, unknown-part-in-message-list |
| PROC-04 | MUST | consumer | A described field carrying a schema-rejected value is fatal | basic/processing.mdx:19-20 | malformed-known-field-fatal, unknown-outcome-array-fatal, subagent-started-missing-name-fatal |
| PROC-05 | MUST | consumer | Fail the run rather than repair, coerce or ignore a malformed known value | basic/processing.mdx:20-22 | malformed-known-field-fatal |
| PROC-06 | MUST | consumer | Same verdict on the same event over every transport (survive as JSON => survive as protobuf; fail => fail same way) | basic/processing.mdx:31-34 | - |
| PROC-07 | implicit | consumer | A reader only turns bytes into events; an undecodable frame is a decode failure; recognition is decided by the shared stage, not the reader | basic/processing.mdx:39-46 | - |
| PROC-08 | MUST | consumer | A binary frame whose envelope variant is unknown is dropped with a warning, not fatal | basic/processing.mdx:47-52 | - |
| PROC-09 | implicit | consumer | Documented exception: protobuf materialises absent required scalars/lists as empty, accepted where JSON would reject | basic/processing.mdx:54-61 | - |
| PROC-10 | MUST NOT | consumer | Deliver unrecognised material to application code as though the protocol described it | basic/processing.mdx:65-66 | unknown-property-stripped |
| PROC-11 | MUST | consumer | Drop (whole event unrecognised) or strip (unknown property / union member on a valid event) | basic/processing.mdx:66-72 | unknown-event-dropped, unknown-property-stripped |
| PROC-12 | SHOULD | consumer | Warn naming the type when dropping an event | basic/processing.mdx:69-70 | unknown-event-dropped |
| PROC-13 | SHOULD | consumer | Warn naming the path when stripping | basic/processing.mdx:71-73 | unknown-property-stripped |
| PROC-14 | MUST NOT | consumer | Let a stripped property survive into app-visible value, or a dropped event reach app code | basic/processing.mdx:75-76 | unknown-event-dropped, unknown-property-stripped |
| PROC-15 | MUST | consumer | Preserve members on objects the schema leaves open (JSON Patch ops: `remove` with leftover `value` arrives whole) | basic/processing.mdx:78-85 | open-object-patch-op-extras |
| PROC-16 | implicit | consumer | Obligation binds the pipeline, not the parser; parser-only SDK layers keep unknown properties (Python/.NET lack a stripping stage today) | basic/processing.mdx:87-92 | - |
| PROC-17 | implicit | consumer | Known gap: unknown value of a closed string set (role, outcome) should be stripped per spec; reference impl is fatal | basic/processing.mdx:94-101 | unknown-enum-value-role-fatal (pins gap) |
| PROC-18 | implicit | consumer | Optional position: remove that field, keep the rest | basic/processing.mdx:106-107 | unknown-outcome-stripped |
| PROC-19 | implicit | consumer | Required position: remove the containing value (e.g. media part with unknown source kind removed whole) | basic/processing.mdx:108-111 | - |
| PROC-20 | implicit | consumer | In a list: drop the unrecognisable element, list survives | basic/processing.mdx:112 | unknown-part-in-message-list, state-delta-unknown-op-dropped |
| PROC-21 | MUST | consumer | Middleware runs before enforcement; enforcement before verification and before app code | basic/processing.mdx:116-118 | era-0-0-45-thinking-translated |
| PROC-22 | MUST | consumer | Compatibility boundary sees the stream first (a retired shape can be a chunk) | basic/processing.mdx:125-128 | era-0-0-45-thinking-translated |
| PROC-23 | MAY | consumer | A middleware chain expands chunks between its own stages | basic/processing.mdx:128-130 | - |
| PROC-24 | MUST | consumer | Middleware handles either chunk or start/content/end form | basic/processing.mdx:131-132 | - |
| PROC-25 | implicit | consumer | Early expansion silently loses unknown chunk fields and chunk `timestamp`; deltas, opener fields, metadata, rawEvent, attribution carried intact | basic/processing.mdx:134-144 | - |
| PROC-26 | MUST | consumer | A chunk is judged as an event: the first stage to meet a malformed chunk rejects it | basic/processing.mdx:146-149 | - |
| PROC-27 | MUST NOT | consumer | Normalise, default or conceal a malformed known value in a chunk | basic/processing.mdx:149-150 | - |
| PROC-28 | MUST | consumer | Verification runs after chunk expansion | basic/processing.mdx:155-157 | chunk-expansion-assembles |
| PROC-29 | MUST | consumer | Middleware sees material before enforcement removes it | basic/processing.mdx:161-164 | era-0-0-45-thinking-translated |
| PROC-30 | MUST NOT | consumer | App code sees material enforcement would remove | basic/processing.mdx:165-167 | unknown-property-stripped |
| PROC-31 | MUST | consumer | Apply the ordering on every event-producing path, including reconnection and replay | basic/processing.mdx:169-171 | - |
| PROC-32 | MUST | consumer | Immediately before transmission, remove unrecognised material from the run input | basic/processing.mdx:175-176 | - |
| PROC-33 | SHOULD | consumer | Warn about material removed from outgoing input | basic/processing.mdx:176-177 | - |
| PROC-34 | MUST | consumer | Treat a malformed known field in outgoing input as fatal before anything is sent | basic/processing.mdx:177-178 | - |
| PROC-35 | MUST | producer | Apply the same asymmetry to received run input (unrecognised OK, malformed fatal) | basic/processing.mdx:180-182 | - |
| PROC-36 | MUST NOT | producer | Emit an event type the protocol does not describe expecting consumers to ignore it (use CUSTOM/RAW) | basic/processing.mdx:186-189 | - |
| PROC-37 | implicit | consumer | (derived) A conformant stream draws no warning: warnings attach to dropped, stripped or lossy material, never to protocol-legal traffic (open members, unknown metadata keys, unrecognised CUSTOM names) | basic/processing.mdx:84-85; basic/metadata.mdx:18-20; events/passthrough.mdx:54-57 | conformant-run-is-quiet |

## 8. basic/versioning.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| VER-01 | MUST | both | Every addition (new event, optional field, open-union member) is safe for an older party to meet | basic/versioning.mdx:11-14 | unknown-event-dropped (partial) |
| VER-02 | MUST NOT | producer | Put in an addition alone anything the protocol already has a place for (outcome, message content, tool call identity/result) | basic/versioning.mdx:16-20 | - |
| VER-03 | MAY | both | Translate (downgrade) the stream for a peer known to be older | basic/versioning.mdx:24-25 | era-0-0-39-flattens-content, era-0-0-57-subagent-dropped-with-warning |
| VER-04 | MUST NOT | both | Invent meaning in a downgrade (non-empty value the producer never sent); empty value for older-required field is allowed reshaping | basic/versioning.mdx:25-29 | - |
| VER-05 | MUST NOT | both | Repair a malformed value in a downgrade | basic/versioning.mdx:31-33 | - |
| VER-06 | MAY | both | Lossless downgrade (e.g. strip `subagentRunId` for a peer without subagents) is silent | basic/versioning.mdx:38-41 | - |
| VER-07 | MUST | both | Lossy downgrade emits a warning naming what was lost and why | basic/versioning.mdx:43-46 | era-0-0-39-flattens-content, era-0-0-57-subagent-dropped-with-warning |
| VER-08 | MUST | both | The warning identifies the shape that was dropped | basic/versioning.mdx:48-49 | era-0-0-57-subagent-dropped-with-warning |
| VER-09 | SHOULD | both | The warning says what to upgrade to remove it | basic/versioning.mdx:49-50 | - |
| VER-10 | MAY | both | Offer a way to silence lossy-downgrade warnings | basic/versioning.mdx:50 | - |
| VER-11 | MUST NOT | both | Silence these warnings by default | basic/versioning.mdx:50 | - |
| VER-12 | MUST | both | A retired shape is recorded in the deprecation registry (replacement, translation location, expiry) | basic/versioning.mdx:58-60 | - |
| VER-13 | SHOULD | consumer | Translate a retired shape to its replacement rather than drop it | basic/versioning.mdx:61-62 | era-0-0-45-thinking-translated, era-0-0-47-upgrades-binary-content |
| VER-14 | MUST | consumer | Retired-shape translation runs as middleware, before enforcement | basic/versioning.mdx:63-65 | era-0-0-45-thinking-translated |
| VER-15 | MUST | consumer | A retired-shape translation that loses content warns | basic/versioning.mdx:66 | era-0-0-39-flattens-content |
| VER-16 | MAY | consumer | Remove a translation after its registry expiry; the shape is then unrecognised | basic/versioning.mdx:68-69 | - |
| VER-17 | MUST | consumer | Declare its version on `RunAgentInput.protocolVersion` (except toward a peer known to predate the field) | basic/versioning.mdx:77-78,84-87,129-130 | current-peer-declares-protocol-version, era-pinned-peer-omits-protocol-version |
| VER-18 | MUST | producer | Declare its own version on `RUN_STARTED.protocolVersion`, never an echo of the input's | basic/versioning.mdx:79-82,84-85,122 | - |
| VER-19 | implicit | both | Both fields optional in the schema; absence identifies a pre-versioning peer | basic/versioning.mdx:83-84 | era-pinned-peer-omits-protocol-version |
| VER-20 | implicit | both | Version = `MAJOR.MINOR`, compared numerically per component (`1.10` > `1.9`) | basic/versioning.mdx:88-91 | producer-declares-newer-version-warns |
| VER-21 | SHOULD | both | A declaration a party cannot interpret is treated like a newer one: proceed and warn | basic/versioning.mdx:90-92 | producer-declares-uninterpretable-version-warns |
| VER-22 | MUST | producer | Serve a run that declares a newer minor of a line it implements, and answer with its own version | basic/versioning.mdx:93-95 | - |
| VER-23 | SHOULD | producer | Warn when serving a newer-minor declaration | basic/versioning.mdx:95 | - |
| VER-24 | MAY | producer | Reject, before RUN_STARTED, only a declaration from a major line it does not implement | basic/versioning.mdx:95-97 | - |
| VER-25 | MUST NOT | producer | Reject a newer minor of its own line for its version | basic/versioning.mdx:97-98 | - |
| VER-26 | SHOULD | consumer | Proceed under the processing model and warn on a newer producer declaration | basic/versioning.mdx:98-99 | producer-declares-newer-version-warns |
| VER-27 | implicit | consumer | Same declared version as the client speaks passes in silence | basic/versioning.mdx:88-99 (negative case, derived) | producer-declares-same-version-quiet |
| VER-28 | implicit | both | A proxy declares what the stream speaks (forwarded untranslated = original; translated = what it emits); each run's RUN_STARTED in a multi-run stream declares its own; live producer does not change mid-stream | basic/versioning.mdx:100-106 | - |
| VER-29 | MAY | transport | Mirror the declaration into transport envelope; in-band field authoritative; binding defines mismatch handling | basic/versioning.mdx:107-110 | - |
| VER-30 | MAY | both | Select a downgrade when the other party's version is known (declared or configured) | basic/versioning.mdx:112-113 | era-0-0-39-flattens-content |
| VER-31 | MUST | both | When the peer's version is unknown, behave as though it were current (no guessing downward) | basic/versioning.mdx:113-115 | era-current-peer-keeps-modern-content (partial) |
| VER-32 | MUST NOT | both | A version-selected downgrade changes outcome, alters identifiers, or reorders events (must be a pure transformation) | basic/versioning.mdx:117-118 | - |
| VER-33 | implicit | producer | (restates BASIC-09) Omit optional fields with no value rather than send `null`; never depend on consumer understanding an addition | basic/versioning.mdx:124-125 | - |
| VER-34 | implicit | consumer | (restates PROC/PIPE) Survive unrecognised, fail on malformed, middleware before enforcement, warn on lossy translation, never present removed material | basic/versioning.mdx:131-136 | - |

## 9. basic/patterns/index.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| PAT-01 | implicit | both | Direction is fixed: events flow producer to consumer; consumer speaks once per exchange | basic/patterns/index.mdx:11-13 | - |
| PAT-02 | MUST NOT | producer | Require any mid-run message from the consumer; outside input arrives on the next run (interrupt+resume, or frontend tool answered in next messages) | basic/patterns/index.mdx:13-18 | - |
| PAT-03 | implicit | transport | Every transport carries all patterns; patterns are expressed only in terms of events and run input | basic/patterns/index.mdx:7-9,76-79 | - |

## 10. basic/patterns/snapshots.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| SNAP-01 | MUST | consumer | Replace its copy with the snapshot's content; a snapshot is not a merge (unless family opt-out: activity `replace:false`) | basic/patterns/snapshots.mdx:19-22 | state-snapshot-replaces, activity-replace-false-preserves |
| SNAP-02 | implicit | consumer | A snapshot, applied or declined, is the new baseline; earlier deltas are spent | basic/patterns/snapshots.mdx:23-24 | state-snapshot-replaces |
| SNAP-03 | MAY | producer | Send a snapshot at any time | basic/patterns/snapshots.mdx:26 | - |
| SNAP-04 | SHOULD | producer | Send a snapshot whenever consumer baseline cannot be guaranteed (after error, inexpressible change, start of run continuing old thread) | basic/patterns/snapshots.mdx:26-29 | - |
| SNAP-05 | implicit | both | Delta = RFC 6902 array applied in order, atomically (any op fails => document unchanged) | basic/patterns/snapshots.mdx:33-37 | state-delta-applies, state-delta-unappliable-warns-and-keeps |
| SNAP-06 | implicit | consumer | Delta applies to the current value = last snapshot as amended by later deltas; for state the start value is input `state` or `{}` | basic/patterns/snapshots.mdx:38-41 | state-delta-applies |
| SNAP-07 | MUST NOT | producer | Send a delta for a value it gave the consumer no baseline for (state: from input; activity: after first snapshot) | basic/patterns/snapshots.mdx:42-44 | - |
| SNAP-08 | MUST NOT | consumer | Strip members RFC 6902 does not define from patch operations (open objects) | basic/patterns/snapshots.mdx:45-47 | open-object-patch-op-extras |
| SNAP-09 | implicit | consumer | Malformed operation (patch not an array, op missing/wrong-typed member) is fatal | basic/patterns/snapshots.mdx:53-55 | - |
| SNAP-10 | implicit | consumer | Operation with an `op` RFC 6902 does not define: drop that element from the patch with a warning; run survives | basic/patterns/snapshots.mdx:55-59 | state-delta-unknown-op-dropped |
| SNAP-11 | MUST NOT | consumer | Keep a partially applied result (atomic) | basic/patterns/snapshots.mdx:62-63 | state-delta-unappliable-warns-and-keeps |
| SNAP-12 | MUST | consumer | Surface a failed well-formed patch as a warning identifying what failed and why | basic/patterns/snapshots.mdx:64-65 | state-delta-unappliable-warns-and-keeps |
| SNAP-13 | MAY | consumer | Continue with prior value rather than fail the run after a patch failure | basic/patterns/snapshots.mdx:65-66 | state-delta-unappliable-warns-and-keeps |
| SNAP-14 | implicit | consumer | A failed `test` operation is the same case: patch does not apply, prior value kept | basic/patterns/snapshots.mdx:67-68 | - |
| SNAP-15 | SHOULD | producer | Resynchronise with a snapshot when it learns of / cannot rule out divergence | basic/patterns/snapshots.mdx:70-72 | - |
| SNAP-16 | MUST | consumer | Adopt the next snapshot regardless of what it skipped before | basic/patterns/snapshots.mdx:72-73 | - |

## 11. basic/patterns/streaming.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| STRM-01 | MUST NOT | producer | Open an item whose identifier is already open | basic/patterns/streaming.mdx:21 | - |
| STRM-02 | MUST NOT | producer | Send a content or end event for an identifier that is not open | basic/patterns/streaming.mdx:22-23 | content-without-start-fatal |
| STRM-03 | MUST | producer | Close every item it opens before the run finishes | basic/patterns/streaming.mdx:24 | open-message-at-run-finished-fatal |
| STRM-04 | implicit | consumer | Opening event carries descriptive fields; content events carry id + `delta`; deltas concatenate in arrival order | basic/patterns/streaming.mdx:26-28 | triad-assembles-message, tool-call-triad-assembles |
| STRM-05 | MAY | producer | Interleave messages, tool calls, reasoning messages and steps, each respecting its own discipline | basic/patterns/streaming.mdx:43-45 | - |
| STRM-06 | MAY | producer | Standalone events (STATE_*, MESSAGES_SNAPSHOT, ACTIVITY_*, CUSTOM, RAW, REASONING_ENCRYPTED_VALUE) appear anywhere in an open run | basic/patterns/streaming.mdx:47-50 | raw-and-custom-pass-through |
| STRM-07 | MUST | consumer | Expand chunk events into start/content/end before verification and application code | basic/patterns/streaming.mdx:57-60 | chunk-expansion-assembles |
| STRM-08 | implicit | consumer | A chunk is judged as the event it is; whichever stage meets a malformed chunk first rejects it | basic/patterns/streaming.mdx:62-64 | - |
| STRM-09 | MUST NOT | producer | Mix the two spellings within one item (chunk-opened item continues in chunk form; `*_END` is synthesized, never sent) | basic/patterns/streaming.mdx:82-89 | - |
| STRM-10 | implicit | consumer | An explicit event carrying the id of an assembling chunk stream ends the shorthand and is judged by triad rules | basic/patterns/streaming.mdx:85-88 | - |
| STRM-11 | MUST | producer | First TEXT_MESSAGE_CHUNK for a message carries `messageId` | basic/patterns/streaming.mdx:96 | first-chunk-missing-id-fatal |
| STRM-12 | MAY | producer | First text chunk may carry `role`; absent means `assistant` | basic/patterns/streaming.mdx:96-98 | chunk-conflicting-role-fatal (partial) |
| STRM-13 | MUST | producer | First REASONING_MESSAGE_CHUNK carries `messageId` | basic/patterns/streaming.mdx:99 | - |
| STRM-14 | MUST | producer | First TOOL_CALL_CHUNK carries both `toolCallId` and `toolCallName` | basic/patterns/streaming.mdx:100-101 | first-tool-chunk-missing-name-fatal |
| STRM-15 | MAY | producer | Later chunks omit opener-required fields and continue what is open | basic/patterns/streaming.mdx:93-94 | chunk-expansion-assembles |
| STRM-16 | MUST | consumer | Treat a first chunk missing a required field as a protocol violation; never invent an identifier | basic/patterns/streaming.mdx:103-104 | first-chunk-missing-id-fatal, first-tool-chunk-missing-name-fatal |
| STRM-17 | MUST | producer | A later chunk carrying an identifier carries the same one it continues | basic/patterns/streaming.mdx:108-109 | - |
| STRM-18 | implicit | consumer | A chunk naming a different identifier opens a new item; previous one closes first | basic/patterns/streaming.mdx:109-110,131 | - |
| STRM-19 | MAY | producer | A continuation chunk repeats `role`/`name` (text) or `toolCallName`/`parentMessageId` (tool) with the same value | basic/patterns/streaming.mdx:111-113 | chunk-repeat-identical-accepted |
| STRM-20 | MUST | consumer | Treat a conflicting repeat as a protocol violation, including conflict with a value established by omission (no role => assistant) | basic/patterns/streaming.mdx:113-116 | chunk-conflicting-role-fatal, chunk-conflicting-tool-name-fatal |
| STRM-21 | implicit | consumer | Synthesize `*_END` when a message/tool-call/step/state/custom/reasoning event arrives in the item's lane; RAW, ACTIVITY_SNAPSHOT/DELTA, REASONING_ENCRYPTED_VALUE and SUBAGENT_STARTED close nothing | basic/patterns/streaming.mdx:132-136 | custom-closes-chunk-stream-fatal, raw-does-not-close-chunk-stream |
| STRM-22 | implicit | consumer | RUN_STARTED/FINISHED/ERROR and MESSAGES_SNAPSHOT close every lane; a subagent terminal closes that subagent's lane | basic/patterns/streaming.mdx:137-139 | chunk-expansion-assembles, subagent-terminal-closes-open-chunk-stream |
| STRM-23 | implicit | consumer | Untagged id-less continuation resolves to the parent agent's open stream of its kind, else the sole open stream of its kind | basic/patterns/streaming.mdx:141-145 | raw-does-not-close-chunk-stream (partial) |
| STRM-24 | MUST | consumer | Reject as ambiguous a continuation with no unique referent (several lanes hold open streams, none the parent's) | basic/patterns/streaming.mdx:145-148 | - |
| STRM-25 | MUST | producer | A producer running parallel subagents attributes its continuations | basic/patterns/streaming.mdx:148 | - |
| STRM-26 | MUST NOT | both | A chunk stream outlives its owner; subagent-opened item closes no later than the subagent's terminal | basic/patterns/streaming.mdx:149-150 | subagent-terminal-closes-open-chunk-stream |
| STRM-27 | implicit | consumer | Chunk `metadata` applies to synthesized events and merges into the item; metadata-only continuation chunk is legal | basic/patterns/streaming.mdx:154-159 | - |

## 12. basic/patterns/interrupt-resume.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| INTR-01 | implicit | producer | A run needing outside input ends with RUN_FINISHED whose outcome is the interrupt outcome carrying at least one Interrupt | basic/patterns/interrupt-resume.mdx:13-16 | run-finished-interrupt-outcome-accepted |
| INTR-02 | MUST NOT | producer | Report an interrupted run as success; interrupt outcome is the only conforming way to end a run that stopped to ask | basic/patterns/interrupt-resume.mdx:18-21 | - |
| INTR-03 | implicit | producer | Frontend-tool stop finishes as success naming `pendingToolCallIds`; a run stopped because told to uses the cancelled outcome | basic/patterns/interrupt-resume.mdx:21-27 | run-finished-cancelled-outcome-accepted (partial) |
| INTR-04 | MUST | producer | Each interrupt `id` unique within the run | basic/patterns/interrupt-resume.mdx:28-29 | - |
| INTR-05 | implicit | producer | `reason` is an open string; `message`, `toolCallId`, `responseSchema` (opaque) are descriptive | basic/patterns/interrupt-resume.mdx:30-34 | - |
| INTR-06 | MAY | producer | Interrupt raised inside a subagent carries that `subagentRunId` | basic/patterns/interrupt-resume.mdx:35-37 | - |
| INTR-07 | implicit | both | An interrupted run is a closed run (late RUN_ERROR admitted); continuing means a new run | basic/patterns/interrupt-resume.mdx:39-42 | run-finished-interrupt-outcome-accepted |
| INTR-08 | MUST | consumer | Each resume entry's `interruptId` names an interrupt of the run being continued (most recent interrupted run on the thread) | basic/patterns/interrupt-resume.mdx:49-50 | - |
| INTR-09 | implicit | consumer | Entry `status` = `resolved` (answered) or `cancelled` (abandoned) per schema.json; `payload` any non-null JSON; `metadata` is envelope info | basic/patterns/interrupt-resume.mdx:51-53 | - |
| INTR-10 | MUST | consumer | Resume list covers every interrupt: each answered or explicitly abandoned; omission is not abandonment | basic/patterns/interrupt-resume.mdx:54-56 | - |
| INTR-11 | MUST | consumer | Reject a resuming input that leaves an interrupt uncovered, before the run starts / before anything is sent | basic/patterns/interrupt-resume.mdx:58-61 | - |
| INTR-12 | MUST NOT | consumer | Silently continue past an interrupt it has no entry for | basic/patterns/interrupt-resume.mdx:61-62 | - |
| INTR-13 | implicit | producer | Producer need not retain the interrupted run, compare lists, or keep inter-run state to be conformant | basic/patterns/interrupt-resume.mdx:62-64,85-90 | - |
| INTR-14 | implicit | consumer | Expiry judged by the consumer reading `expiresAt` (reference: date, now-or-earlier is expired); consumer rejects an entry answering an expired interrupt before the run starts | basic/patterns/interrupt-resume.mdx:67-73 | - |
| INTR-15 | implicit | consumer | An expired interrupt can (and by coverage must) still be abandoned | basic/patterns/interrupt-resume.mdx:73-75 | - |
| INTR-16 | implicit | both | Resuming run keeps same `threadId`, accumulated messages and the state left behind | basic/patterns/interrupt-resume.mdx:76-79 | - |
| INTR-17 | implicit | producer | Resuming run's `usage` covers only its own model calls | basic/patterns/interrupt-resume.mdx:80-83 | - |
| INTR-18 | SHOULD | producer | Unrecognised resume entry (names an interrupt it did not raise): proceed without it | basic/patterns/interrupt-resume.mdx:95-97 | - |
| INTR-19 | SHOULD | producer | Surface a warning for an unrecognised entry rather than fail | basic/patterns/interrupt-resume.mdx:96-97 | - |
| INTR-20 | MUST NOT | producer | Perform the interrupted action on the strength of an absent entry | basic/patterns/interrupt-resume.mdx:103-104 | - |
| INTR-21 | MUST NOT | producer | Treat omission as abandonment (incl. dropping the call with a warning and finishing success) | basic/patterns/interrupt-resume.mdx:104-107 | - |
| INTR-22 | MAY | producer | On a noticed uncovered interrupt: reject input before the run starts, or run covered entries and end with the interrupt outcome again carrying the still-open interrupt | basic/patterns/interrupt-resume.mdx:107-112 | - |
| INTR-23 | MUST NOT | producer | End as success while an interrupt it knows to be open stands unanswered | basic/patterns/interrupt-resume.mdx:112-114 | - |
| INTR-24 | implicit | both | Success outcome carrying interrupts is rejected by schema; resume list on a non-continuing run is treated as unrecognised entries | basic/patterns/interrupt-resume.mdx:144-147 | - |

## 13. basic/transports/index.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| TRN-01 | MUST | transport | Binding provides ordered, complete delivery of events in emission order | basic/transports/index.mdx:16-20 | - |
| TRN-02 | MUST | transport | Binding delivers the RunAgentInput before any events | basic/transports/index.mdx:16,21-22 | - |
| TRN-03 | implicit | transport | Replayed extra runs get no further input; each RUN_STARTED MAY carry its own `input` echo | basic/transports/index.mdx:22-26 | - |
| TRN-04 | MUST | transport | Provide a termination signal a consumer can tell from truncation | basic/transports/index.mdx:16,27-29 | - |
| TRN-05 | MUST | transport | Provide an error path refusing invalid RunAgentInput before RUN_STARTED, outside the stream | basic/transports/index.mdx:16,30-31 | - |
| TRN-06 | implicit | transport | Authentication/authorization belong to the binding/application; AG-UI defines no credential | basic/transports/index.mdx:33-36 | - |
| TRN-07 | MUST | transport | An implementation that speaks HTTP supports the SSE binding | basic/transports/index.mdx:46-47 | - |
| TRN-08 | OPTIONAL | transport | The protobuf binding | basic/transports/index.mdx:47 | - |
| TRN-09 | MUST NOT | consumer | Synthesize a RUN_FINISHED for a truncated run (stream ended without terminal event; run has no outcome) | basic/transports/index.mdx:51-54 | - |
| TRN-10 | MUST NOT | consumer | Report a truncated run as having succeeded | basic/transports/index.mdx:54-55 | - |
| TRN-11 | implicit | consumer | Events delivered before the break stay delivered; re-running is a new run with a new `runId` | basic/transports/index.mdx:55-58 | - |
| TRN-12 | MAY | transport | Carry AG-UI over custom channels (WebSocket, buses, pipes) | basic/transports/index.mdx:62-63 | - |
| TRN-13 | MUST | transport | A custom transport preserves event model, patterns and processing rules and satisfies the binding contract | basic/transports/index.mdx:63-65 | - |
| TRN-14 | SHOULD | transport | A custom transport documents framing, input delivery, termination and error signals | basic/transports/index.mdx:65-66 | - |
| TRN-15 | SHOULD | transport | A custom JSON transport frames events as the SSE binding does: one event object per frame | basic/transports/index.mdx:68-71 | - |

## 14. basic/transports/http-sse.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| SSE-01 | implicit | consumer | Client sends POST; body is the RunAgentInput as one UTF-8 JSON object, `Content-Type: application/json` | basic/transports/http-sse.mdx:13-15 | - |
| SSE-02 | implicit | consumer | Client sends `Accept: text/event-stream` (may add the protobuf media type) | basic/transports/http-sse.mdx:16-18 | - |
| SSE-03 | implicit | producer | A run that starts answers `200` with `Content-Type: text/event-stream` | basic/transports/http-sse.mdx:22 | - |
| SSE-04 | implicit | producer | Each SSE event's `data` is exactly one protocol event JSON object, never several, never a fragment; multi-line `data:` joins per SSE | basic/transports/http-sse.mdx:23-25 | - |
| SSE-05 | MUST | producer | Frame the stream with LF line endings | basic/transports/http-sse.mdx:26-27 | - |
| SSE-06 | MAY | consumer | Additionally accept CR and CRLF | basic/transports/http-sse.mdx:28-29 | - |
| SSE-07 | MUST | consumer | Ignore SSE fields other than `data` (`event:`, `id:`, `retry:`) | basic/transports/http-sse.mdx:30-31 | - |
| SSE-08 | MUST | consumer | Tolerate SSE comment lines (`: keep-alive`) | basic/transports/http-sse.mdx:31-32 | - |
| SSE-09 | MAY | producer | Send comment lines at any cadence | basic/transports/http-sse.mdx:32 | - |
| SSE-10 | implicit | producer | Close the response body after the last run's terminal event | basic/transports/http-sse.mdx:33 | - |
| SSE-11 | MAY | producer | A response carries several runs (replay ahead of the requested run) | basic/transports/http-sse.mdx:34-38 | - |
| SSE-12 | implicit | producer | Input rejected before start (bad JSON, validation, auth) is an HTTP error status with no event stream | basic/transports/http-sse.mdx:42-44 | - |
| SSE-13 | implicit | producer | A failure after the stream opens travels in-stream as RUN_ERROR | basic/transports/http-sse.mdx:45-46 | - |
| SSE-14 | MUST NOT | consumer | Infer success from `200` alone | basic/transports/http-sse.mdx:46-47 | - |
| SSE-15 | implicit | consumer | A connection dropped without a terminal event is a truncated run | basic/transports/http-sse.mdx:48-49 | - |
| SSE-16 | implicit | transport | No stream resumption: `Last-Event-ID` unused; re-run is a new run with a new `runId` | basic/transports/http-sse.mdx:53-55 | - |

## 15. basic/transports/http-protobuf.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| PB-01 | implicit | consumer | To opt in, include `application/vnd.ag-ui.event+proto` in `Accept` alongside `text/event-stream`; admission (incl. `*/*`, `application/*`) is what opts in | basic/transports/http-protobuf.mdx:12-15 | - |
| PB-02 | MUST | consumer | A client that cannot consume protobuf sends an explicit `Accept` naming what it can, not a wildcard or down-ranked entry | basic/transports/http-protobuf.mdx:15-18 | - |
| PB-03 | implicit | producer | Relative quality values between the two media types are not consulted | basic/transports/http-protobuf.mdx:17-18 | - |
| PB-04 | SHOULD | producer | A producer supporting the binding answers with it whenever `Accept` admits the type with positive quality | basic/transports/http-protobuf.mdx:19-21 | - |
| PB-05 | MUST | producer | Answer SSE otherwise | basic/transports/http-protobuf.mdx:19-21 | - |
| PB-06 | MUST | consumer | Always be prepared to receive SSE (a producer lacking proto ignores the type) | basic/transports/http-protobuf.mdx:21-22 | - |
| PB-07 | implicit | producer | Response `Content-Type` is exactly `application/vnd.ag-ui.event+proto`; consumer selects parser by it | basic/transports/http-protobuf.mdx:23-24 | - |
| PB-08 | implicit | transport | Frame = 4-byte unsigned 32-bit big-endian length + exactly that many bytes of one encoded event; no separator | basic/transports/http-protobuf.mdx:28-32 | - |
| PB-09 | MUST | consumer | Tolerate frames split across transport chunks and multiple frames per chunk | basic/transports/http-protobuf.mdx:32-33 | - |
| PB-10 | implicit | consumer | A body ending mid-frame is a truncated run (not a decode failure) | basic/transports/http-protobuf.mdx:34-35,68-69 | - |
| PB-11 | implicit | transport | Wire schema is generated from the same JSON Schema; not a second source of truth | basic/transports/http-protobuf.mdx:39-41 | - |
| PB-12 | MUST | producer | Every first-party encoder reproduces the canonical byte corpus byte for byte | basic/transports/http-protobuf.mdx:42-44 | - |
| PB-13 | implicit | consumer | Decoded events enter the same pipeline as SSE events (middleware first, enforcement after) | basic/transports/http-protobuf.mdx:50-52 | - |
| PB-14 | MUST NOT | consumer | Reject an unrecognised `RUN_FINISHED.outcome` (or other open payload) at decode time; enforcement strips with warning as on SSE | basic/transports/http-protobuf.mdx:53-57 | unknown-outcome-stripped (JSON lane only) |
| PB-15 | implicit | consumer | Wire-schema-unknown fields are skipped silently by protobuf decoding (accepted narrowing) | basic/transports/http-protobuf.mdx:58-60 | - |
| PB-16 | MUST | consumer | JSON wire preserves JSON Patch operation extension members (protobuf cannot) | basic/transports/http-protobuf.mdx:61-63 | open-object-patch-op-extras |
| PB-17 | implicit | consumer | A frame whose envelope arm this build predates is dropped with a warning (spelled at the transport) | basic/transports/http-protobuf.mdx:64-66 | - |
| PB-18 | implicit | consumer | A frame whose bytes do not decode as a message is malformed transport material and fatal to the stream | basic/transports/http-protobuf.mdx:67-68 | - |
| PB-19 | implicit | both | Errors identical to SSE: pre-stream rejection = HTTP error status; in-stream failure = RUN_ERROR frame | basic/transports/http-protobuf.mdx:73-75 | - |

## 16. events/index.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| EVT-01 | implicit | producer | Only the run lifecycle is mandatory; every other family is emitted when there is something to say | events/index.mdx:23-25 | - |
| EVT-02 | MUST | consumer | Accept all eight families; an event from a family with no use is well-formed, not unrecognised material | events/index.mdx:25-27 | - |

## 17. events/lifecycle.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| LIFE-01 | MUST | producer | A stream begins with RUN_STARTED or RUN_ERROR (RUN_ERROR admitted first: run can fail before it begins) | events/lifecycle.mdx:14 | first-event-not-run-started-fatal, run-error-first-run-continues |
| LIFE-02 | MUST NOT | producer | Emit any other event first | events/lifecycle.mdx:14-15 | first-event-not-run-started-fatal |
| LIFE-03 | MUST | consumer | Treat a stream opening with anything else as a protocol violation | events/lifecycle.mdx:15-16 | first-event-not-run-started-fatal |
| LIFE-04 | implicit | producer | RUN_STARTED carries run identifiers, own `protocolVersion`, optional input echo, optional `parentRunId` (agent invoked as a separate run, not a subagent) | events/lifecycle.mdx:33-40 | current-peer-declares-protocol-version (partial) |
| LIFE-05 | implicit | both | RUN_FINISHED closes a non-failed run; `outcome` distinguishes completed / interrupted / stopped; absent outcome = success | events/lifecycle.mdx:44-48 | run-finished-interrupt-outcome-accepted, run-finished-cancelled-outcome-accepted |
| LIFE-06 | MAY | producer | Success outcome names unanswered frontend calls in `pendingToolCallIds` | events/lifecycle.mdx:53-56 | - |
| LIFE-07 | MUST NOT | producer | Send an outcome value the schema does not describe | events/lifecycle.mdx:57 | - |
| LIFE-08 | OPTIONAL | producer | `result` (return value) and `usage` (one TokenUsage per provider+model) on RUN_FINISHED | events/lifecycle.mdx:58-61 | - |
| LIFE-09 | implicit | consumer | Unrecognised outcome is stripped; stripped optional field leaves it absent, absent outcome = success | events/lifecycle.mdx:63-67 | unknown-outcome-stripped |
| LIFE-10 | MUST | producer | A run stopped before completion for a non-failure reason closes with RUN_FINISHED carrying the cancelled outcome | events/lifecycle.mdx:87-89 | run-finished-cancelled-outcome-accepted (consumer side) |
| LIFE-11 | MUST NOT | producer | Report a stopped run as success (neither success outcome nor omitted outcome) | events/lifecycle.mdx:89-90 | - |
| LIFE-12 | MUST | producer | Close everything the run opened (messages, tool calls, steps, subagents) before the cancelling RUN_FINISHED | events/lifecycle.mdx:91-94 | - |
| LIFE-13 | implicit | producer | A producer that cannot close in order (stream torn down) reports RUN_ERROR instead | events/lifecycle.mdx:98-100 | - |
| LIFE-14 | SHOULD | producer | Cancelled run's `result` is absent | events/lifecycle.mdx:101 | - |
| LIFE-15 | MAY | producer | Cancelled run's `usage` reports tokens accrued before the stop | events/lifecycle.mdx:101-102 | - |
| LIFE-16 | implicit | producer | Cancelled outcome carries no interrupts; next run on the thread is an ordinary new run, not a resume | events/lifecycle.mdx:103-104 | - |
| LIFE-17 | MUST NOT | consumer | Present a cancelled run as having succeeded | events/lifecycle.mdx:105 | run-finished-cancelled-outcome-accepted (partial) |
| LIFE-18 | MUST NOT | consumer | Present a cancelled run as having failed | events/lifecycle.mdx:105-107 | - |
| LIFE-19 | implicit | consumer | Everything delivered before a stop / RUN_ERROR remains delivered | events/lifecycle.mdx:107-109,241-243 | run-finished-cancelled-outcome-accepted, run-error-then-new-run-continues |
| LIFE-20 | MUST NOT | consumer | Synthesize a RUN_FINISHED (cancelled or otherwise) for a stream it abandoned (truncated, not cancelled) | events/lifecycle.mdx:111-115 | - |
| LIFE-21 | implicit | producer | RUN_ERROR: `message` human-readable; `code` OPTIONAL open string, no vocabulary; `usage` MAY report pre-failure tokens | events/lifecycle.mdx:119-122 | - |
| LIFE-22 | implicit | consumer | RUN_ERROR is a well-formed event: consumer accepts it, does not reject the stream | events/lifecycle.mdx:124-126,237-239 | run-error-first-run-continues, run-error-then-new-run-continues |
| LIFE-23 | MUST | consumer | Surface a RUN_ERROR failure to application code (mechanism is implementation-defined) | events/lifecycle.mdx:126-130 | late-run-error-after-finished-run-continues, run-error-first-run-continues |
| LIFE-24 | MUST NOT | consumer | Report a run that ended in RUN_ERROR as having succeeded | events/lifecycle.mdx:126-127 | late-run-error-after-finished-run-continues |
| LIFE-25 | implicit | producer | `inputTokens` = every prompt token charged (cached or not); `outputTokens` = every generated token incl. reasoning | events/lifecycle.mdx:142-144 | - |
| LIFE-26 | implicit | producer | `cachedInputTokens`, `cacheWriteInputTokens`, `reasoningTokens` are parts of the totals, never additions; the two cache counts are disjoint | events/lifecycle.mdx:145-147 | - |
| LIFE-27 | MUST | producer | Add a part into the total when the provider reports it beside a smaller total | events/lifecycle.mdx:147-148 | - |
| LIFE-28 | MUST NOT | producer | Add a part again when the provider's total already includes it | events/lifecycle.mdx:148-149 | - |
| LIFE-29 | implicit | producer | `totalTokens` = `inputTokens` + `outputTokens` | events/lifecycle.mdx:150 | - |
| LIFE-30 | MAY | producer | Compute `totalTokens` rather than copy the provider's | events/lifecycle.mdx:150-151 | - |
| LIFE-31 | MUST NOT | producer | Copy a provider total that counts differently | events/lifecycle.mdx:151-152 | - |
| LIFE-32 | MUST NOT | producer | Emit a zero for a count it has no data for (absent = not reported; zero = reported zero) | events/lifecycle.mdx:153-155 | - |
| LIFE-33 | MUST NOT | consumer | Read an absent count as zero | events/lifecycle.mdx:155 | - |
| LIFE-34 | implicit | producer | Usage covers all model calls in the run incl. subagents'; subagent events carry no usage | events/lifecycle.mdx:159-161 | - |
| LIFE-35 | MUST NOT | producer | Include in a run's usage the usage of an agent invoked as a separate run (named by `parentRunId`) | events/lifecycle.mdx:162-164 | - |
| LIFE-36 | implicit | producer | A resuming run reports only the calls it made itself | events/lifecycle.mdx:165-168 | - |
| LIFE-37 | MUST NOT | producer | After a run closes, emit any further event for it other than RUN_STARTED (new run) or a late RUN_ERROR | events/lifecycle.mdx:177-181 | event-after-run-finished-fatal |
| LIFE-38 | MAY | producer | Emit RUN_STARTED to begin a new run on the same stream | events/lifecycle.mdx:182 | two-sequential-runs-accumulate, run-error-then-new-run-continues |
| LIFE-39 | MAY | producer | Emit RUN_ERROR after RUN_FINISHED for a late failure (e.g. flush error) | events/lifecycle.mdx:183-185 | late-run-error-after-finished-run-continues |
| LIFE-40 | MUST | consumer | Treat the run as failed after a late RUN_ERROR | events/lifecycle.mdx:184-185 | late-run-error-after-finished-run-continues |
| LIFE-41 | MUST NOT | producer | Emit anything after RUN_ERROR except RUN_STARTED | events/lifecycle.mdx:186 | - |
| LIFE-42 | MUST | consumer | Reject any other event arriving after a run has closed | events/lifecycle.mdx:188 | event-after-run-finished-fatal |
| LIFE-43 | MAY | producer | A stream carries several runs in sequence (replayed thread) | events/lifecycle.mdx:192-193 | two-sequential-runs-accumulate |
| LIFE-44 | MUST | producer | Close the current run before opening the next; RUN_STARTED while a run is active is a violation | events/lifecycle.mdx:193-194 | nested-run-started-fatal |
| LIFE-45 | implicit | consumer | Across runs on a stream, messages accumulate and state persists unless replaced | events/lifecycle.mdx:196-197 | two-sequential-runs-accumulate |
| LIFE-46 | MUST | producer | Restate replayed history as snapshots (MESSAGES_SNAPSHOT reconciles by id; STATE_SNAPSHOT replaces), not by re-streaming | events/lifecycle.mdx:198-203 | - |
| LIFE-47 | implicit | consumer | Re-streaming a message the consumer holds appends to it | events/lifecycle.mdx:201-203 | reopen-closed-message-appends |
| LIFE-48 | implicit | consumer | Run-scoped tracking (open messages, calls, steps, subagents) resets per run; RUN_ERROR ends whatever was open; new run starts with nothing open | events/lifecycle.mdx:205-208 | run-error-then-new-run-continues |
| LIFE-49 | MUST NOT | producer | Open a step whose name is already open | events/lifecycle.mdx:215 | - |
| LIFE-50 | MUST NOT | producer | Finish a step that was never opened | events/lifecycle.mdx:215-216 | step-finished-without-start-fatal |
| LIFE-51 | MUST | producer | Close every opened step before the run finishes | events/lifecycle.mdx:217 | - |
| LIFE-52 | MAY | producer | Steps overlap each other and anything else (a label over a span, not a container) | events/lifecycle.mdx:218-219 | - |
| LIFE-53 | implicit | consumer | Fatal on every producer MUST NOT above: event before RUN_STARTED, post-close event, nested RUN_STARTED, unbalanced step, anything open at RUN_FINISHED | events/lifecycle.mdx:231-235 | first-event-not-run-started-fatal, event-after-run-finished-fatal, nested-run-started-fatal, step-finished-without-start-fatal, open-message-at-run-finished-fatal |
| LIFE-54 | MUST NOT | consumer | Present a rejected stream and a reported RUN_ERROR failure as one another | events/lifecycle.mdx:241 | - |

## 18. events/text-messages.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| TEXT-01 | MAY | consumer | Buffer a whole message before showing it, or render incrementally | events/text-messages.mdx:14-15 | - |
| TEXT-02 | implicit | consumer | Streaming-pattern rules apply to messages matched by `messageId` | events/text-messages.mdx:19-21 | triad-assembles-message |
| TEXT-03 | OPTIONAL | producer | TEXT_MESSAGE_START `role` (absent = `assistant`) and `name` | events/text-messages.mdx:35-38 | - |
| TEXT-04 | implicit | consumer | TEXT_MESSAGE_CONTENT `delta`s concatenate in arrival order | events/text-messages.mdx:42-43 | triad-assembles-message |
| TEXT-05 | MAY | producer | Reopen a closed `messageId` with a new TEXT_MESSAGE_START; further content appends | events/text-messages.mdx:55-58 | reopen-closed-message-appends |
| TEXT-06 | MUST | producer | A reopening START agrees with the message (same owner, role, name) | events/text-messages.mdx:58-60 | - |
| TEXT-07 | implicit | consumer | Consumer need not detect a reopen disagreement; the established values stand | events/text-messages.mdx:59-61,65-70 | - |
| TEXT-08 | MAY | producer | A later MESSAGES_SNAPSHOT restates a closed message wholesale; no other means changes a closed message's content | events/text-messages.mdx:61-63 | messages-snapshot-reconciles-by-id |
| TEXT-09 | MUST | consumer | Expand TEXT_MESSAGE_CHUNK per the streaming pattern incl. first-chunk requirement and conflicting-repeat rule | events/text-messages.mdx:75-78 | chunk-expansion-assembles, chunk-conflicting-role-fatal |
| TEXT-10 | implicit | consumer | Metadata on any of a message's events merges into the message (key by key, last wins) | events/text-messages.mdx:106-108 | metadata-merges-last-write-wins |
| TEXT-11 | implicit | consumer | CONTENT/END for a non-open id, or START for an open id, is fatal; message open at run finish is a violation | events/text-messages.mdx:112-116 | content-without-start-fatal, open-message-at-run-finished-fatal |
| TEXT-12 | implicit | consumer | The assembled message appears in history as an Assistant/User/System/Developer message according to its role | events/text-messages.mdx:102-104 | triad-assembles-message (partial: assistant only) |

## 19. events/tool-calls.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| TOOL-01 | OPTIONAL | producer | `parentMessageId` attaches the call to the carrying assistant message | events/tool-calls.mdx:37-38 | tool-call-triad-assembles |
| TOOL-02 | MUST | producer | When the parent message is attributed to a subagent, the call agrees with that attribution | events/tool-calls.mdx:38-40 | - |
| TOOL-03 | implicit | consumer | TOOL_CALL_ARGS deltas concatenate into argument text (conventionally JSON) which the protocol does not validate | events/tool-calls.mdx:44-48 | tool-call-triad-assembles |
| TOOL-04 | MUST NOT | consumer | Act on arguments before TOOL_CALL_END | events/tool-calls.mdx:50-52 | - |
| TOOL-05 | implicit | consumer | TOOL_CALL_END establishes completeness, not validity; unparseable text is the application's decision, not a protocol violation | events/tool-calls.mdx:56-59 | - |
| TOOL-06 | MAY | producer | Reopen a closed call with TOOL_CALL_START of the same `toolCallId`; arguments append | events/tool-calls.mdx:61-62 | - |
| TOOL-07 | MUST | producer | A reopening START agrees with the call (same `toolCallName`, `parentMessageId`, owner) | events/tool-calls.mdx:63-65 | - |
| TOOL-08 | implicit | consumer | Consumer need not detect disagreement and the protocol makes no promise which value wins (unlike text messages) | events/tool-calls.mdx:65-68 | - |
| TOOL-09 | MUST | consumer | Expand TOOL_CALL_CHUNK; first chunk carries `toolCallId` and `toolCallName` | events/tool-calls.mdx:72-75 | chunk-expansion-assembles, first-tool-chunk-missing-name-fatal |
| TOOL-10 | implicit | consumer | A continuation repeating `toolCallName`/`parentMessageId` with a conflicting value is fatal | events/tool-calls.mdx:75-76 | chunk-conflicting-tool-name-fatal |
| TOOL-11 | implicit | consumer | TOOL_CALL_RESULT is a tool message with its own `messageId`; does not reopen the call | events/tool-calls.mdx:80-81 | tool-result-parts-mint-tool-message |
| TOOL-12 | MAY | producer | A result arrives in the same run (agent-executed) or never in-stream (client-executed; returns as tool message in next input) | events/tool-calls.mdx:92-94 | - |
| TOOL-13 | MUST | producer | Result `content` is a string or an ordered list of ContentPart | events/tool-calls.mdx:98-100 | tool-result-parts-mint-tool-message |
| TOOL-14 | implicit | consumer | The tool message minted has identical content shape; a result travels into next `messages` unchanged | events/tool-calls.mdx:104-105 | tool-result-parts-mint-tool-message |
| TOOL-15 | implicit | producer | Structured data is serialised into the string or a `text` part; there is no JSON part; extras ride part `metadata` | events/tool-calls.mdx:123-131 | - |
| TOOL-16 | implicit | consumer | A `file`-source handle in a result stays opaque and passes through unchanged | events/tool-calls.mdx:132-136 | tool-result-file-source-passes-through |
| TOOL-17 | MUST NOT | producer | Fail the run because a result part cannot be taken by the model; drop the part and continue | events/tool-calls.mdx:137-141 | - |
| TOOL-18 | MUST | producer | Still answer the call; a result with every part dropped is answered with the empty string | events/tool-calls.mdx:141-144 | - |
| TOOL-19 | implicit | consumer | A string-only consumer renders `text` parts concatenated in order, ignoring the rest | events/tool-calls.mdx:145-147 | - |
| TOOL-20 | SHOULD | consumer | Announce that string-flattening is lossy | events/tool-calls.mdx:147-148 | - |
| TOOL-21 | MAY | producer | Flatten a result for a peer that sent no `protocolVersion` | events/tool-calls.mdx:149-151 | era-0-0-39-flattens-content (partial) |
| TOOL-22 | MUST | producer | Warn when flattening dropped a part | events/tool-calls.mdx:151-152 | era-0-0-39-flattens-content (partial) |
| TOOL-23 | MUST NOT | producer | Put a placeholder in a dropped part's place | events/tool-calls.mdx:152-153 | - |
| TOOL-24 | MUST NOT | producer | Answer a frontend tool call (no TOOL_CALL_RESULT, no fabricated tool message) | events/tool-calls.mdx:162-164 | - |
| TOOL-25 | MUST | producer | Finish the run with the frontend call unanswered, using the success outcome or none | events/tool-calls.mdx:165-166 | - |
| TOOL-26 | MUST NOT | producer | Report a frontend-tool stop as interrupted | events/tool-calls.mdx:166-167 | - |
| TOOL-27 | SHOULD | producer | Finish promptly once nothing remains that does not depend on the result | events/tool-calls.mdx:167-168 | - |
| TOOL-28 | MAY | producer | Leave several frontend calls unanswered in one run | events/tool-calls.mdx:168-169 | - |
| TOOL-29 | implicit | producer | `pendingToolCallIds` names unanswered calls in the order made | events/tool-calls.mdx:170-171 | - |
| TOOL-30 | SHOULD | producer | Send `pendingToolCallIds` when the run leaves calls pending | events/tool-calls.mdx:171-172 | - |
| TOOL-31 | MUST | producer | When sent, the list contains exactly the calls the run started and did not answer with TOOL_CALL_RESULT | events/tool-calls.mdx:172-173 | - |
| TOOL-32 | MUST NOT | consumer | Read an absent/empty list as "nothing pending"; derive pending calls from the stream (also for interrupted runs) | events/tool-calls.mdx:173-179 | - |
| TOOL-33 | MUST | consumer | A thread that continues answers every unanswered frontend call: next `messages` carry a tool message per call keyed by `toolCallId` (failure = tool message with `error`; declined = says so) | events/tool-calls.mdx:180-187 | - |
| TOOL-34 | implicit | consumer | Such a tool message's `content` is a string or list of parts; abandoning the thread violates nothing | events/tool-calls.mdx:187-193 | - |
| TOOL-35 | SHOULD | producer | Call frontend tools only from the advertised list; an unadvertised call is not itself a violation (consumer decides) | events/tool-calls.mdx:213-216 | - |
| TOOL-36 | MUST NOT | producer | Emit `ReasoningPart`, `ToolCallPart`, `AssistantPart` (reserved for a later minor) | events/tool-calls.mdx:248-250 | - |
| TOOL-37 | implicit | consumer | Continuing/closing a non-open call, or reopening an open one, is fatal; call open at run finish is a violation | events/tool-calls.mdx:254-256 | - |
| TOOL-38 | MUST | consumer | Treat tool arguments as untrusted: validate against declared parameter schema, never interpolate unescaped | events/tool-calls.mdx:263-266 | - |
| TOOL-39 | SHOULD | consumer | Obtain user consent before executing a side-effectful tool call | events/tool-calls.mdx:267-268 | - |
| TOOL-40 | MUST NOT | consumer | Represent a call as user-approved when it was not | events/tool-calls.mdx:268 | - |
| TOOL-41 | MUST NOT | consumer | Treat text inside a tool result as protocol material or as instructions with the user's authority | events/tool-calls.mdx:269-271 | - |
| TOOL-42 | SHOULD NOT | consumer | Execute an unadvertised tool without the scrutiny a new tool would get | events/tool-calls.mdx:272-273 | - |
| TOOL-43 | implicit | consumer | Arguments of a reopened call append to what the call already holds (consumer side of TOOL-06) | events/tool-calls.mdx:61-62 | - |

## 20. events/state.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| STATE-01 | MUST | consumer | Replace state with `snapshot` on STATE_SNAPSHOT, no merging | events/state.mdx:15-17 | state-snapshot-replaces, state-falsy-snapshot-applies, state-null-snapshot-applies |
| STATE-02 | implicit | consumer | STATE_DELTA baseline = input `state` at run start, then the run's own snapshots/deltas; pattern's atomicity, open ops, failure handling apply | events/state.mdx:21-26 | state-delta-applies |
| STATE-03 | implicit | consumer | `State` is any JSON value, not only an object; falsy values are not absent | events/state.mdx:79-81 | state-falsy-snapshot-applies, state-null-snapshot-applies |
| STATE-04 | implicit | consumer | MESSAGES_SNAPSHOT replaces the consumer's copy with the same id in place (keeps position) | events/state.mdx:35-37 | messages-snapshot-reconciles-by-id |
| STATE-05 | implicit | consumer | Unseen snapshot messages are appended in snapshot order; snapshot order does not reorder held messages | events/state.mdx:37-40 | - |
| STATE-06 | implicit | consumer | Consumer-held messages absent from the snapshot are dropped | events/state.mdx:41-42 | messages-snapshot-reconciles-by-id |
| STATE-07 | implicit | consumer | Exception: activity and reasoning messages (client-only) are not dropped when absent | events/state.mdx:42-45 | - |
| STATE-08 | implicit | consumer | Exception is per-role and self-revoking: a snapshot carrying any activity (resp. reasoning) message declares that role's complete set | events/state.mdx:46-49 | - |
| STATE-09 | implicit | producer | MESSAGES_SNAPSHOT carries no `subagentRunId`; ownership comes from contained messages | events/state.mdx:51-52 | - |
| STATE-10 | implicit | both | State persists across runs until replaced; next input carries it back; messages accumulate likewise | events/state.mdx:56-60 | - |
| STATE-11 | implicit | consumer | Malformed patch is fatal; well-formed patch that fails to apply is warned and skipped | events/state.mdx:85-87 | state-delta-unappliable-warns-and-keeps |
| STATE-12 | MUST | consumer | Validate what is read out of state before acting on it (render, execute, grant) | events/state.mdx:91-94 | - |
| STATE-13 | SHOULD NOT | producer | Put secrets in state | events/state.mdx:94-96 | - |
| STATE-14 | implicit | producer | MESSAGES_SNAPSHOT carries the complete set of messages the producer owns, in order | events/state.mdx:30 | messages-snapshot-reconciles-by-id (partial) |

## 21. events/activity.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| ACT-01 | MUST | consumer | Tolerate `activityType` values it does not recognise (open string) | events/activity.mdx:29-30 | - |
| ACT-02 | implicit | consumer | Snapshot for an unseen `messageId` creates the activity message at its arrival position | events/activity.mdx:31-32 | activity-snapshot-then-delta |
| ACT-03 | implicit | consumer | Snapshot for an existing activity replaces content and `activityType`; absent `replace` means replace (normative) | events/activity.mdx:33-35 | - |
| ACT-04 | implicit | consumer | `replace: false` leaves the existing message (content and `activityType`) as it stands; snapshot values apply only on creation; not a merge | events/activity.mdx:35-38 | activity-replace-false-preserves |
| ACT-05 | implicit | consumer | Replacement replaces content, not accumulated metadata (metadata keeps merging) | events/activity.mdx:39-41 | - |
| ACT-06 | implicit | consumer | A replacing snapshot re-mints the activity: message `subagentRunId` becomes the snapshot's own, including absent | events/activity.mdx:41-43 | - |
| ACT-07 | implicit | consumer | A `replace: false` snapshot does not re-mint ownership; established owner stands | events/activity.mdx:44-46 | - |
| ACT-08 | MUST | producer | A `replace: false` snapshot's attribution agrees with the established owner (consumer not required to reject) | events/activity.mdx:45-50 | - |
| ACT-09 | implicit | consumer | ACTIVITY_DELTA's `activityType` replaces the message's (delta MAY retype) | events/activity.mdx:54-56 | - |
| ACT-10 | MUST | producer | A delta that does not intend to retype repeats the current `activityType` (field required) | events/activity.mdx:56-57 | - |
| ACT-11 | MUST NOT | producer | Send a delta for an activity message not created by a snapshot | events/activity.mdx:59-61 | - |
| ACT-12 | implicit | consumer | A delta naming a missing or non-activity message is skipped (no message invented) | events/activity.mdx:62-64 | activity-delta-missing-target-tolerated |
| ACT-13 | SHOULD | consumer | Surface a warning for that skipped delta (fixture does not assert it) | events/activity.mdx:63 | - |
| ACT-14 | MUST NOT | consumer | Fail the run over a delta for a missing/non-activity message | events/activity.mdx:63-64 | activity-delta-missing-target-tolerated |
| ACT-15 | MUST | producer | Patched `content` still an object; do not send a patch (e.g. root replace) that makes it anything else; consumer not required to detect | events/activity.mdx:65-68 | - |
| ACT-16 | implicit | consumer | Patch failure handling is the snapshot pattern's; resync via fresh snapshot of same `messageId` | events/activity.mdx:69-71 | - |
| ACT-17 | implicit | both | Activity message appears in history with fixed role `activity` | events/activity.mdx:75 | - |
| ACT-18 | MUST | consumer | Strip activity messages from `messages` sent in run input | events/activity.mdx:76-78 | - |
| ACT-19 | implicit | consumer | Malformed activity events are fatal; delta-to-missing and non-applying patch warn and skip | events/activity.mdx:105-109 | - |

## 22. events/reasoning.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| REAS-01 | MAY | consumer | Hide reasoning entirely | events/reasoning.mdx:14-15 | - |
| REAS-02 | MUST NOT | producer | Open a span whose `messageId` is already open | events/reasoning.mdx:25 | - |
| REAS-03 | MUST | producer | Close every opened span before the run finishes | events/reasoning.mdx:25-26 | - |
| REAS-04 | MUST NOT | producer | Close a span it did not open | events/reasoning.mdx:26-27 | - |
| REAS-05 | MAY | producer | A span contains several reasoning messages; span id namespaces nothing | events/reasoning.mdx:22-23,28-29 | reasoning-span-and-message |
| REAS-06 | implicit | both | REASONING_MESSAGE_START `role` is fixed `reasoning` (other value is malformed, fatal) | events/reasoning.mdx:35 | reasoning-message-wrong-role-fatal |
| REAS-07 | MUST | producer | First REASONING_MESSAGE_CHUNK carries `messageId` | events/reasoning.mdx:37-38 | - |
| REAS-08 | MUST | consumer | Detect streaming-discipline violations on spans and reasoning messages and fail the run | events/reasoning.mdx:41-42 | reasoning-discipline-verified |
| REAS-09 | MUST NOT | consumer | Invent the message an orphaned REASONING_MESSAGE_CONTENT/END names | events/reasoning.mdx:44-47 | reasoning-discipline-verified |
| REAS-10 | implicit | consumer | REASONING_END for an unopened span, or REASONING_START reopening an open span, is rejected | events/reasoning.mdx:48-49 | - |
| REAS-11 | implicit | consumer | REASONING_MESSAGE_START for an already-open reasoning message is rejected | events/reasoning.mdx:50-52 | - |
| REAS-12 | implicit | consumer | Span and reasoning message MAY share an identifier; each checked against its own open set; a fragment naming a span's id is orphaned | events/reasoning.mdx:52-53,57-59 | - |
| REAS-13 | implicit | consumer | A span or reasoning message still open at run finish fails the run | events/reasoning.mdx:54-55 | - |
| REAS-14 | MUST | consumer | Treat `encryptedValue` as opaque: never parsed, interpreted or format-assumed | events/reasoning.mdx:67-68 | - |
| REAS-15 | MUST | consumer | Store the value with the message or tool call `entityId` names (by `subtype`) and return it unmodified in later run input | events/reasoning.mdx:63-65,69-71 | - |
| REAS-16 | implicit | consumer | An event naming an activity message (cannot hold a value) is the unknown-`entityId` case | events/reasoning.mdx:71-74 | - |
| REAS-17 | MUST | consumer | Warn when unable to preserve it (downgrade path, store that cannot carry it) | events/reasoning.mdx:75-77 | - |
| REAS-18 | MUST | producer | Restate the artefact in a MESSAGES_SNAPSHOT copy if still needed; omitting withdraws it; consumer does not resurrect it | events/reasoning.mdx:78-83 | - |
| REAS-19 | MAY | producer | REASONING_ENCRYPTED_VALUE appears anywhere in an open run (standalone, opens/closes nothing) | events/reasoning.mdx:85-87 | - |
| REAS-20 | SHOULD | consumer | Warn on an `entityId` naming nothing known | events/reasoning.mdx:115-117 | - |
| REAS-21 | MAY | consumer | Drop such a value | events/reasoning.mdx:116-117 | encrypted-value-unknown-entity-tolerated |
| REAS-22 | MUST NOT | consumer | Fail the run over an unknown `entityId` | events/reasoning.mdx:117 | encrypted-value-unknown-entity-tolerated |
| REAS-23 | implicit | consumer | Malformed reasoning event (missing field, non-string `subtype`) fatal; string `subtype` the schema does not name is unrecognised: stripped with warning, which in a required position removes the event | events/reasoning.mdx:119-124 | - |
| REAS-24 | SHOULD | consumer | Treat reasoning content with the same confidentiality as the conversation | events/reasoning.mdx:128-130 | - |
| REAS-25 | MUST NOT | consumer | Feed `encryptedValue` to anything but the producer that issued it | events/reasoning.mdx:130-132 | - |
| REAS-26 | implicit | consumer | After a MESSAGES_SNAPSHOT replaces a message, the consumer returns the encrypted value it holds once all events have applied; it does not resurrect one the snapshot removed | events/reasoning.mdx:78-83 | - |

## 23. events/subagents.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| SUB-01 | implicit | producer | Events a subagent produces carry its `subagentRunId`; the parent's do not; absence means parent | events/subagents.mdx:12-14 | - |
| SUB-02 | MUST NOT | producer | Spell an absent `subagentRunId` as `null` | events/subagents.mdx:14 | - |
| SUB-03 | MUST NOT | producer | Reuse a `subagentRunId` for a second invocation within a run | events/subagents.mdx:16-18 | - |
| SUB-04 | MAY | producer | An id reappears in a later run when a suspended invocation is continued | events/subagents.mdx:18-19 | - |
| SUB-05 | implicit | consumer | A tag on a standalone event (STEP_*, CUSTOM, RAW, state) is provenance, not a private copy | events/subagents.mdx:21-25 | - |
| SUB-06 | MUST NOT | producer | SUBAGENT_STARTED/FINISHED/ERROR carry the `subagentRunId` they concern: a producer MUST NOT omit it | events/subagents.mdx:29-31 | - |
| SUB-07 | MUST | producer | A producer that announces an invocation does so before any event attributed to it | events/subagents.mdx:33-34 | - |
| SUB-08 | MUST NOT | producer | Announce an invocation whose id is already active | events/subagents.mdx:35 | - |
| SUB-09 | MUST NOT | producer | Reuse the id of an invocation already finished in this run | events/subagents.mdx:36-37 | - |
| SUB-10 | MUST | producer | Close every announced invocation with SUBAGENT_FINISHED or SUBAGENT_ERROR before the run finishes | events/subagents.mdx:38-40 | run-finished-with-active-subagent-fatal |
| SUB-11 | MAY | producer | Attribute events without announcing anything | events/subagents.mdx:41 | subagent-unannounced-attribution-accepted |
| SUB-12 | MUST | consumer | Accept attribution without lifecycle events | events/subagents.mdx:42-44 | subagent-unannounced-attribution-accepted |
| SUB-13 | SHOULD | producer | A producer that can report the lifecycle does | events/subagents.mdx:45-47 | - |
| SUB-14 | implicit | consumer | SUBAGENT_ERROR ends the invocation, not the run (parent may continue) | events/subagents.mdx:49-50 | - |
| SUB-15 | implicit | both | No subagent event carries token usage; run's terminal event reports whole-run usage | events/subagents.mdx:52-54 | - |
| SUB-16 | implicit | consumer | SUBAGENT_FINISHED: absent outcome = success; suspended = paused awaiting input, terminal for this stream, continuable by a later run under the same id | events/subagents.mdx:56-59 | - |
| SUB-17 | MUST NOT | producer | Send an outcome the schema does not describe; attach interrupt ids to a success outcome | events/subagents.mdx:59-61 | - |
| SUB-18 | MUST | producer | Once an entity is opened under an owner, every continuation agrees about that owner | events/subagents.mdx:65-67 | subagent-attribution-mismatch-fatal (partial) |
| SUB-19 | MUST NOT | producer | TEXT_MESSAGE_CONTENT/END carry a `subagentRunId` disagreeing with the START | events/subagents.mdx:68-70 | - |
| SUB-20 | MAY | producer | Omit the tag on a continuation; untagged continues whatever the opener owns | events/subagents.mdx:70-71 | - |
| SUB-21 | implicit | both | Same ownership rule for reasoning messages, tool calls (vs TOOL_CALL_START) and activity messages (vs opening snapshot) | events/subagents.mdx:72-74 | subagent-attribution-mismatch-fatal (partial: tool-call continuation) |
| SUB-22 | MUST | producer | STEP_FINISHED carries the same attribution as its STEP_STARTED | events/subagents.mdx:75-76 | - |
| SUB-23 | MAY | producer | Parent and subagent each hold a same-named step open; each closes its own | events/subagents.mdx:76-77 | - |
| SUB-24 | MUST | consumer | Reject a continuation whose tag disagrees with its opener | events/subagents.mdx:79-81 | subagent-attribution-mismatch-fatal |
| SUB-25 | MUST | consumer | Accept a continuation carrying no tag | events/subagents.mdx:81-82 | - |
| SUB-26 | MUST NOT | producer | Attribute a tool call to a different owner than the message carrying it (inherits the message's attribution) | events/subagents.mdx:84-86 | - |
| SUB-27 | MAY | producer | A subagent delegates further; `parentSubagentRunId` on SUBAGENT_STARTED names the spawner (absent = parent agent) | events/subagents.mdx:90-92 | - |
| SUB-28 | MUST NOT | producer | Name a parent invocation not announced in this run | events/subagents.mdx:92-93 | - |
| SUB-29 | MAY | producer | Invocations run in parallel with arbitrarily interleaved events; entities tracked per invocation | events/subagents.mdx:95-97 | - |
| SUB-30 | MUST NOT | consumer | Assume one invocation's events are contiguous, or close one invocation's entities because another ended | events/subagents.mdx:97-99 | - |
| SUB-31 | MUST | consumer | Apply a subagent's STATE_SNAPSHOT/DELTA to the run's state like the parent's (no per-subagent state) | events/subagents.mdx:103-105,110 | - |
| SUB-32 | MUST NOT | consumer | Read `subagentRunId` on a state event as private state, or withhold the update because a subagent sent it | events/subagents.mdx:107-110 | - |
| SUB-33 | MUST NOT | producer | Finish a run while an invocation is still active | events/subagents.mdx:114-115 | run-finished-with-active-subagent-fatal |
| SUB-34 | MUST | consumer | Treat every open invocation as abandoned on RUN_ERROR, and not expect a closing event | events/subagents.mdx:119-121 | - |
| SUB-35 | MAY | producer | Interrupts on an interrupt-outcome RUN_FINISHED are attributed (tagged = raised in that invocation; untagged = parent); a suspended outcome MAY carry no interrupt ids | events/subagents.mdx:123-127 | - |
| SUB-36 | implicit | consumer | SUBAGENT_FINISHED for a never-announced invocation is fatal (no explicit sentence in the spec; fixture rationale only) | events/subagents.mdx:29-30 | subagent-finished-unstarted-fatal |

## 24. events/passthrough.mdx

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| PASS-01 | implicit | both | RAW and CUSTOM are standalone, open/close nothing, appear anywhere in an open run; CUSTOM (not RAW) ends an open chunk stream in its lane | events/passthrough.mdx:6-11 | raw-and-custom-pass-through, custom-closes-chunk-stream-fatal, raw-does-not-close-chunk-stream |
| PASS-02 | REQUIRED | producer | RAW `event` (any JSON value); `source` OPTIONAL | events/passthrough.mdx:28-29 | - |
| PASS-03 | MUST NOT | consumer | Derive protocol behaviour from a RAW event | events/passthrough.mdx:30-31 | raw-does-not-close-chunk-stream |
| PASS-04 | SHOULD | producer | Emit RAW alongside the standard events, never instead of them | events/passthrough.mdx:32-34 | - |
| PASS-05 | REQUIRED | producer | CUSTOM `name` and `value` | events/passthrough.mdx:52-53 | - |
| PASS-06 | MUST | consumer | Ignore a CUSTOM event with an unrecognised `name`; nothing stripped, nothing warns | events/passthrough.mdx:54-57 | raw-and-custom-pass-through |
| PASS-07 | SHOULD | producer | Prefix invented CUSTOM names with a vendor/application identifier; unprefixed names are reserved for the protocol | events/passthrough.mdx:58-61 | - |
| PASS-08 | MUST NOT | producer | Rely on CUSTOM to carry semantics the spec assigns to a standard event | events/passthrough.mdx:63-65 | - |
| PASS-09 | MUST | consumer | Treat RAW `event` and CUSTOM `value` as untrusted: never rendered as markup, executed, or granted authority | events/passthrough.mdx:76-77 | - |
| PASS-10 | SHOULD | consumer | Treat a RAW payload with the suspicion due the network traffic it summarises | events/passthrough.mdx:77-78 | - |

## 25. changelog.mdx (1.0)

Informative only (changelog.mdx:6-9: "It is informative: the linked pages are the specification"); it contains no independent normative rules. Items of note it records (all covered above): 31 event types; `subagents` key replaces `subAgents` (not read) (changelog.mdx:70-72); part renames `InputContent`->`ContentPart` etc., wire unchanged, SDK aliases kept (changelog.mdx:79-84); every part gains optional `id`, text part gains `metadata` (changelog.mdx:84-86); `file` source new in 1.0 (changelog.mdx:109-111).

Rev 2: the changelog does carry capitalised keywords and one behaviour stated nowhere else (`subAgents` is not read), so its sentences are now rows. Restatements are classified `informative` in `compliance.toml` and point at the row that owns the rule.

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| CHG-01 | MUST | producer | (restates STRM-11/14/15/20) First chunks carry what opening requires; later chunks MAY omit it; a conflicting repeat is fatal | changelog.mdx:54-58 | first-chunk-missing-id-fatal, first-tool-chunk-missing-name-fatal |
| CHG-02 | MAY | producer | (restates LIFE-08, LIFE-25..36) RUN_FINISHED and RUN_ERROR carry per-provider token usage in one accounting | changelog.mdx:98-103 | - |
| CHG-03 | MAY | producer | (restates LIFE-06, TOOL-25/26) A frontend-tool stop finishes as success and MAY name the calls in `pendingToolCallIds` | changelog.mdx:104-107 | - |
| CHG-04 | MAY | consumer | Input messages carry multimodal content parts (text, image, audio, video, document) by URL, inline data or provider file handle | changelog.mdx:108-111 | unknown-part-in-message-list (partial) |
| CHG-05 | implicit | consumer | The 0.x `subAgents` capabilities key is not read (`subagents` replaces it) | changelog.mdx:70-72 | - |
| CHG-06 | implicit | producer | Every producer written before outcomes existed is already conformant (absent outcome = success) | changelog.mdx:27 | - |

## 30. schema-files.mdx

Appended in rev 2 (the section number is out of page order so no existing section moves).

| ID | kw | party | rule | src | fixture |
|---|---|---|---|---|---|
| SCHF-01 | implicit | consumer | The strict schema is for authoring and tests; a runtime consumer must not wire it into its receive path (it accepts and strips instead) | schema-files.mdx:38-45 | unknown-property-stripped |
| SCHF-02 | implicit | both | (restates PROC-15, SNAP-08) The schema closes objects whose shape it fixes; `metadata` and JSON Patch operations stay open | schema-files.mdx:38-42 | open-object-patch-op-extras |

## 26. Draft (post-1.0) changes

Sources: `docs/spec/draft/changelog.mdx:17-60` ("Changes in 1.1", minor changes and schema changes at lines 34-76), `docs/spec/draft/events/reasoning.mdx:90-116`, `docs/spec/draft/events/tool-calls.mdx:160-220`, and `diff spec/draft/schema.json spec/1.0/schema.json`. No other draft `.mdx` page differs from 1.0 beyond link prefixes, the draft notice and descriptions (checked by normalising `/spec/draft` to `/spec/1.0` and diffing every page). Draft changelog says "Major changes: None yet" and 1.1 "only adds" (draft/changelog.mdx:23,30-32).

Schema diff (draft vs 1.0, exhaustive): exactly two additive optional properties, both strings:
- `encryptedValueType` on `ReasoningEncryptedValueEvent`, on every message type that carries `encryptedValue`, and on `ToolCall` (5 schema locations).
- `error` on `ToolCallResultEvent` (`ToolMessage.error` already exists in 1.0).
- Protobuf freeze diff: appends `Message.encrypted_value_type = 14`, `ReasoningEncryptedValueEvent.encrypted_value_type = 6`, `ToolCall.encrypted_value_type = 6`, `ToolCallResultEvent.error = 8`; no field number changes.
- Draft conformance streams add 3 fixtures: `tool-result-empty-error-preserved`, `tool-result-error-cleared-by-correction`, `tool-result-error-survives-snapshot`.

Rust implication: a 1.0-only Rust client must strip both fields with a warning on JSON (PROC-13) and skip silently on protobuf (PB-15); a lenient parser may keep them. Both are forward-compatible by design, so modelling them as `Option<String>` now is safe.

| ID | kw | party | rule (draft / 1.1) | src (draft docs) | fixture (draft) |
|---|---|---|---|---|---|
| DRAFT-01 | SHOULD | producer | Prefix `encryptedValueType` values with provider/framework (`anthropic.redacted_thinking`, `openai.encrypted_content`) | draft/events/reasoning.mdx:101-104 | - |
| DRAFT-02 | MUST | consumer | Treat `encryptedValueType` as opaque, stored with the value, returned unmodified | draft/events/reasoning.mdx:105-106 | - |
| DRAFT-03 | MUST | consumer | When storing a new `encryptedValue`, store that event's `encryptedValueType` with it | draft/events/reasoning.mdx:107-109 | - |
| DRAFT-04 | MUST | consumer | Remove the stored type when the new event carries none (no stale type on new bytes) | draft/events/reasoning.mdx:109-110 | - |
| DRAFT-05 | MUST NOT | producer | Send `encryptedValueType` without `encryptedValue` | draft/events/reasoning.mdx:111 | - |
| DRAFT-06 | MUST | producer | Accept an `encryptedValue` without a type in later run input | draft/events/reasoning.mdx:112-114 | - |
| DRAFT-07 | MAY | producer | Drop a value whose type it does not recognise rather than send provider bytes it did not issue | draft/events/reasoning.mdx:115-116 | - |
| DRAFT-08 | MUST NOT | producer | Send an empty `TOOL_CALL_RESULT.error` | draft/events/tool-calls.mdx:182-183 | - |
| DRAFT-09 | SHOULD | producer | Say so in words in `error` when the tool gave no reason | draft/events/tool-calls.mdx:184-185 | - |
| DRAFT-10 | implicit | consumer | A non-empty `error` string marks the tool call (not the run) as failed; run outcome untouched | draft/events/tool-calls.mdx:180-183 | - |
| DRAFT-11 | implicit | consumer | An empty `error` is still read as a failure with no reason; schema accepts it on purpose | draft/events/tool-calls.mdx:186-188 | draft:tool-result-empty-error-preserved |
| DRAFT-12 | SHOULD | consumer | Warn on an empty `error` | draft/events/tool-calls.mdx:187 | draft:tool-result-empty-error-preserved (partial) |
| DRAFT-13 | MUST NOT | consumer | Treat an absent `error` as proof of success | draft/events/tool-calls.mdx:192-195 | - |
| DRAFT-14 | SHOULD | producer | Make `content` describe the failure; `error` travels alongside `content`, never instead of it | draft/events/tool-calls.mdx:196-199 | - |
| DRAFT-15 | MUST | consumer | Copy `error` onto the minted tool message unchanged, empty string included | draft/events/tool-calls.mdx:201-202 | draft:tool-result-empty-error-preserved |
| DRAFT-16 | MUST | producer | A restating MESSAGES_SNAPSHOT carries the `error` sent on the event, unless correcting the result | draft/events/tool-calls.mdx:203-205 | draft:tool-result-error-survives-snapshot |
| DRAFT-17 | implicit | consumer | A corrected restatement carries new content and whatever `error` now applies (or none); snapshot replaces the message so an old `error` does not outlive a correction | draft/events/tool-calls.mdx:205-210 | draft:tool-result-error-cleared-by-correction |
| DRAFT-18 | MAY | producer | Leave `error` off the event for an older peer; that downgrade is lossy | draft/events/tool-calls.mdx:216-217 | - |
| DRAFT-19 | MUST | producer | Warn when doing so (versioning rules) | draft/events/tool-calls.mdx:217-218 | - |
| DRAFT-20 | SHOULD | producer | Set `error` on any tool message it restates for such a peer | draft/events/tool-calls.mdx:218-220 | - |

## 27. Ambiguities, gaps and tensions found

- A1. Unknown media-part `source.type`: run-input.mdx:83-86 says the source arms are closed and an unrecognised type "fails validation rather than being carried through"; processing.mdx:108-111 says the same case is stripped by removing the whole part (to avoid a fatal error). These can be reconciled only if "fails validation" means "is not carried through" (stripped), not "is fatal". No fixture pins it (unknown-part-in-message-list covers only an unknown part kind).
- A2. Known gap acknowledged by the spec (processing.mdx:94-101): unknown value of a closed string set (message `role`, outcome) should be stripped, but the reference is fatal; fixture `unknown-enum-value-role-fatal` pins the non-conforming behaviour. A Rust SDK following the spec will fail that fixture's expectation. Also, a stripped required-position `subtype` removes the whole event (reasoning.mdx:121-124).
- A3. "Lane" in chunk closing (streaming.mdx:131-148) is used both as per-owner (parent vs subagent) and as per-kind ("open stream of its kind"); whether a TEXT chunk stream is closed by a TOOL_CALL_START in the same owner lane is implied (STRM-21) but only CUSTOM and RAW have fixtures.
- A4. Subagent announce rule is conditional: SUB-07 (MUST announce before attributed events) only applies to producers that announce; SUB-11/12 allow attribution with no lifecycle at all.
- A5. Uninterpretable `protocolVersion` (versioning.mdx:90-92) is "handled like a newer one: proceed, SHOULD warn", while a producer MAY reject a declaration from an unimplemented major line (95-97). A producer cannot know the major of an uninterpretable value, so which rule wins is unspecified.
- A6. Reopening after END: text reopening START "MUST agree" and established values stand (text-messages.mdx:58-61), but tool-call reopening gives "no promise which value" (tool-calls.mdx:65-68). Intentional asymmetry, but unspecified outcome for tool calls. **Rev 2 resolution:** text reopen keeps the established values (spec text) and Rust warns; tool-call reopen follows TS `apply/default.ts:828-834` (warn, latest name wins); an owner mismatch on either is fatal within a run (SUB-24, TS `verify/verify.ts:403-408`). See `compliance/README.md` A6.
- A7. RUN_ERROR admitted at start, after RUN_FINISHED and mid-run; lifecycle state diagram (lifecycle.mdx:20-29) shows Closed->Failed but LIFE-41 forbids anything after RUN_ERROR except RUN_STARTED, so two consecutive RUN_ERRORs are forbidden while RUN_ERROR after RUN_ERROR-closed run is not drawn; consistent but only implicit.
- A8. Interrupt coverage/expiry (INTR-11, INTR-14) are consumer-side MUSTs with no stream fixture (they act on the input before sending); no producer coverage check is required (INTR-13), yet INTR-21/23 bind a producer that can tell. The expiry parse format is deliberately unspecified (ISO 8601 only "conventionally").
- A9. Producer-side MUST NOTs are largely unfixtured because the fixture harness feeds consumer-side streams; only violations a consumer must reject have fixtures (see Section 28).
- A10. `processing.mdx:87-92` states Python and .NET do not yet satisfy the strip rule (parser only); the spec admits SDK non-conformance, so the TS client is the only reference for PROC-10/14 behaviour.
- A11. PB-14 (protobuf must not reject unknown outcome) is only fixture-tested on the JSON lane (`unknown-outcome-stripped`); the byte corpus in `spec/1.0/fixtures` was not part of this ledger.
- A12. Duplicated rules across pages (not contradictory): activity-message stripping (RUNIN-12 / ACT-18), no-synthetic-RUN_FINISHED (TRN-09 / LIFE-20), protocolVersion declaration (RUNIN-08 / VER-17), absent-not-null (BASIC-09 / VER-33).
- A13. (rev 2) Arguments after a tool-call reopen: the spec says further arguments append (tool-calls.mdx:61-62; LIFE-47 says re-streaming appends). TS replaces the stored arguments on the first TOOL_CALL_ARGS after a reopening START (`apply/default.ts:821,933-937`) to make replays idempotent. Spec text wins: append (TOOL-43). Upstream issue to be filed.
- A14. (rev 2) PB-14 is fixtured on the JSON lane only; it becomes genuinely fixtured when the protobuf lane replays the stream corpus over protobuf.
- A15. (rev 2) Producer-party rows cite consumer fixtures. A fatal fixture is a must-refuse input for the producer verifier; a completed fixture is a producer-legal example only if it is not among audit-1 C2's 12 producer-illegal streams. Neither proves a producer emits correctly.

## 28. Rules with no conformance fixture

Rules whose fixture column is `-` (362 of 516 non-draft rows; regenerated in rev 2). Fixture "(partial)" rules are not listed here.

- IDX (12): IDX-01, IDX-02, IDX-03, IDX-04, IDX-05, IDX-06, IDX-07, IDX-08, IDX-09, IDX-10, IDX-11, IDX-12
- ARCH (3): ARCH-01, ARCH-04, ARCH-06
- BASIC (16): BASIC-01, BASIC-02, BASIC-03, BASIC-04, BASIC-05, BASIC-06, BASIC-07, BASIC-08, BASIC-09, BASIC-12, BASIC-13, BASIC-14, BASIC-15, BASIC-17, BASIC-18, BASIC-19
- RUNIN (29): RUNIN-01, RUNIN-02, RUNIN-03, RUNIN-04, RUNIN-05, RUNIN-06, RUNIN-07, RUNIN-09, RUNIN-10, RUNIN-11, RUNIN-12, RUNIN-14, RUNIN-15, RUNIN-16, RUNIN-17, RUNIN-18, RUNIN-19, RUNIN-20, RUNIN-21, RUNIN-22, RUNIN-23, RUNIN-24, RUNIN-25, RUNIN-26, RUNIN-27, RUNIN-28, RUNIN-29, RUNIN-30, RUNIN-31
- CAP (16): CAP-01, CAP-02, CAP-03, CAP-04, CAP-05, CAP-06, CAP-07, CAP-08, CAP-09, CAP-10, CAP-11, CAP-12, CAP-13, CAP-14, CAP-15, CAP-16
- META (10): META-02, META-04, META-05, META-09, META-11, META-12, META-13, META-14, META-15, META-16
- PROC (17): PROC-06, PROC-07, PROC-08, PROC-09, PROC-16, PROC-19, PROC-23, PROC-24, PROC-25, PROC-26, PROC-27, PROC-31, PROC-32, PROC-33, PROC-34, PROC-35, PROC-36
- VER (19): VER-02, VER-04, VER-05, VER-06, VER-09, VER-10, VER-11, VER-12, VER-16, VER-18, VER-22, VER-23, VER-24, VER-25, VER-28, VER-29, VER-32, VER-33, VER-34
- PAT (3): PAT-01, PAT-02, PAT-03
- SNAP (7): SNAP-03, SNAP-04, SNAP-07, SNAP-09, SNAP-14, SNAP-15, SNAP-16
- STRM (11): STRM-01, STRM-05, STRM-08, STRM-09, STRM-10, STRM-13, STRM-17, STRM-18, STRM-24, STRM-25, STRM-27
- INTR (21): INTR-02, INTR-04, INTR-05, INTR-06, INTR-08, INTR-09, INTR-10, INTR-11, INTR-12, INTR-13, INTR-14, INTR-15, INTR-16, INTR-17, INTR-18, INTR-19, INTR-20, INTR-21, INTR-22, INTR-23, INTR-24
- TRN (15): TRN-01, TRN-02, TRN-03, TRN-04, TRN-05, TRN-06, TRN-07, TRN-08, TRN-09, TRN-10, TRN-11, TRN-12, TRN-13, TRN-14, TRN-15
- SSE (16): SSE-01, SSE-02, SSE-03, SSE-04, SSE-05, SSE-06, SSE-07, SSE-08, SSE-09, SSE-10, SSE-11, SSE-12, SSE-13, SSE-14, SSE-15, SSE-16
- PB (17): PB-01, PB-02, PB-03, PB-04, PB-05, PB-06, PB-07, PB-08, PB-09, PB-10, PB-11, PB-12, PB-13, PB-15, PB-17, PB-18, PB-19
- EVT (2): EVT-01, EVT-02
- LIFE (30): LIFE-06, LIFE-07, LIFE-08, LIFE-11, LIFE-12, LIFE-13, LIFE-14, LIFE-15, LIFE-16, LIFE-18, LIFE-20, LIFE-21, LIFE-25, LIFE-26, LIFE-27, LIFE-28, LIFE-29, LIFE-30, LIFE-31, LIFE-32, LIFE-33, LIFE-34, LIFE-35, LIFE-36, LIFE-41, LIFE-46, LIFE-49, LIFE-51, LIFE-52, LIFE-54
- TEXT (4): TEXT-01, TEXT-03, TEXT-06, TEXT-07
- TOOL (33): TOOL-02, TOOL-04, TOOL-05, TOOL-06, TOOL-07, TOOL-08, TOOL-12, TOOL-15, TOOL-17, TOOL-18, TOOL-19, TOOL-20, TOOL-23, TOOL-24, TOOL-25, TOOL-26, TOOL-27, TOOL-28, TOOL-29, TOOL-30, TOOL-31, TOOL-32, TOOL-33, TOOL-34, TOOL-35, TOOL-36, TOOL-37, TOOL-38, TOOL-39, TOOL-40, TOOL-41, TOOL-42, TOOL-43
- STATE (7): STATE-05, STATE-07, STATE-08, STATE-09, STATE-10, STATE-12, STATE-13
- ACT (15): ACT-01, ACT-03, ACT-05, ACT-06, ACT-07, ACT-08, ACT-09, ACT-10, ACT-11, ACT-13, ACT-15, ACT-16, ACT-17, ACT-18, ACT-19
- REAS (20): REAS-01, REAS-02, REAS-03, REAS-04, REAS-07, REAS-10, REAS-11, REAS-12, REAS-13, REAS-14, REAS-15, REAS-16, REAS-17, REAS-18, REAS-19, REAS-20, REAS-23, REAS-24, REAS-25, REAS-26
- SUB (28): SUB-01, SUB-02, SUB-03, SUB-04, SUB-05, SUB-06, SUB-07, SUB-08, SUB-09, SUB-13, SUB-14, SUB-15, SUB-16, SUB-17, SUB-19, SUB-20, SUB-22, SUB-23, SUB-25, SUB-26, SUB-27, SUB-28, SUB-29, SUB-30, SUB-31, SUB-32, SUB-34, SUB-35
- PASS (7): PASS-02, PASS-04, PASS-05, PASS-07, PASS-08, PASS-09, PASS-10
- CHG (4): CHG-02, CHG-03, CHG-05, CHG-06

Fixtures that match no ledger row: none (rev 2 mapped subagent-started-missing-name-fatal → PROC-04, era-0-0-47-upgrades-binary-content → VER-13, conformant-run-is-quiet → PROC-37).

## 29. Counts

Regenerated from the tables in rev 2 (the v1 figures 489 / 323 / 341 were stale against v1's own table, which had 501 / 330 / 353).

- Main ledger rows (excluding DRAFT): **516**. By keyword: MAY=54, MUST=124, MUST NOT=98, OPTIONAL=12, REQUIRED=5, SHOULD=36, SHOULD NOT=3, implicit=184.
- Draft rows: 20. By keyword: MAY=2, MUST=7, MUST NOT=3, SHOULD=5, implicit=3.
- RFC-keyword rows (MUST, MUST NOT, SHOULD, SHOULD NOT, MAY): 315; REQUIRED/OPTIONAL: 17; implicit: 184.
- MUST/SHOULD-class rows (MUST, MUST NOT, REQUIRED, SHOULD, SHOULD NOT): **266**.
- By party: both=52, consumer=245, producer=201, transport=18.
- Rows with a fixture (full or partial): 154; with none: **362**.
- Classification totals (sdk-ownable / sdk-assistable / application / informative) are in `compliance/README.md`.
