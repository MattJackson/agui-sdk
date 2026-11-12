//! The protobuf binding (`transports/http-protobuf.mdx`), written for this
//! crate with no protobuf library dependency.
//!
//! * [`encode`] / [`decode`] translate between an event's JSON form and the
//!   wire messages generated from the same schema (PB-11). The codec is driven
//!   by static tables describing the upstream `.proto` files, whose field
//!   numbers are protocol facts pinned by `proto-freeze.txt`; a test parses the
//!   pinned `.proto` files and the freeze file and fails on any drift.
//! * [`framing`] is the 4-byte big-endian length prefix (PB-08), with an
//!   incremental decoder that tolerates frames split across reads and several
//!   frames per read (PB-09), refuses an oversized frame from its header alone,
//!   and tells a truncated body apart from a malformed one (PB-10).
//!
//! Decoded events enter the same pipeline as SSE events: middleware first,
//! enforcement after (PB-13). The codec therefore produces JSON, never typed
//! events, and never judges recognition: an unknown envelope arm is reported
//! as [`decode::Decoded::UnknownArm`] for the pipeline to drop with a warning
//! (PROC-08, PB-17), unknown fields inside a known message are skipped
//! silently (PB-15), open payloads such as `RUN_FINISHED.outcome` pass through
//! uninspected (PB-14), and undecodable bytes are fatal (PB-18).
//!
//! # JSON mapping
//!
//! * `type` selects the `Event` envelope arm; `timestamp`, `rawEvent` and
//!   `metadata` travel in the arm's `base_event` and are flattened back on
//!   decode.
//! * Outcome objects (`RUN_FINISHED`, `SUBAGENT_FINISHED`) are folded into the
//!   message's `outcome` string and its sibling fields.
//! * `content` is a string, a part list or (activity messages) an object, each
//!   on its own wire field.
//! * Open JSON (`state`, `metadata`, `rawEvent`, `CUSTOM.value`, …) uses
//!   `google.protobuf.Value`/`Struct`. Numbers are JS numbers (`f64`).
//! * Absent and `null` typed members are both left off the wire, and absent
//!   fields decode as absent, never `null`. Required strings and lists that
//!   protobuf cannot tell from empty are materialised as `""`/`[]` (PROC-09).
//!   A JSON Patch operation's extension members do not survive (PB-16).
//! * An enum number this build has no value for (a newer patch `op`) is not
//!   malformed bytes: it decodes as the string `#<number>`, which no enum
//!   value spells, so enforcement drops the element with the same warning an
//!   unknown `op` gets on the JSON wire (D3, PROC-06). The encoder maps
//!   `#<number>` back to the number, so such a frame re-encodes unchanged.
//!
//! # Byte parity and open-object order
//!
//! `google.protobuf.Struct` entries are written in JavaScript property order
//! (array-index keys ascending, then insertion order; a repeated key keeps its
//! first position and its last value). That is what the canonical 98-file
//! byte corpus pins (PB-12).
//!
//! * [`encode::encode_json`] reads the event from JSON text, so it knows the
//!   insertion order: it reproduces the corpus byte for byte in every build.
//! * [`encode::encode_value`] and [`encode::encode_event`] start from
//!   `serde_json` maps. Their order is insertion order only when the crate
//!   feature **`preserve-order`** is enabled (it turns on
//!   `serde_json/preserve_order`); then they are byte-identical to the corpus
//!   too. Without it, maps iterate in sorted key order and the output is
//!   conformant (the spec lets encoders order open-object entries
//!   differently) but not always byte-identical.
//! * [`decode::decode_event`] builds a `serde_json::Value`, so the same
//!   feature decides whether the decoded objects keep wire order.
//!   [`decode::decode_event_json`] writes JSON text in wire order in every
//!   build.
//!
//! The feature is opt-in because Cargo unifies features: enabling
//! `serde_json/preserve_order` changes map order for every crate in the
//! dependency graph that uses `serde_json`.

mod codec_error;
mod decoder;
mod encoder;
mod json;
mod schema;
mod wire;

#[cfg(test)]
mod schema_sync;

pub mod decode;
pub mod encode;
// `fields.rs`, the table `cargo xtask codegen` generates from the `.proto`
// files, is not part of the library: the codec reads `schema`, and
// `tests/spec_drift.rs` includes the generated file by path.
pub mod framing;

pub use crate::wire::PROTO_MEDIA_TYPE as MEDIA_TYPE;
