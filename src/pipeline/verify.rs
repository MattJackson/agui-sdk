//! Stream verification for both roles.
//!
//! [`ConsumerVerifier`] runs in the client pipeline after chunk expansion
//! (PROC-28). It rejects every producer violation a consumer must detect:
//! a first event other than `RUN_STARTED`/`RUN_ERROR` (LIFE-03), events after a
//! closed run other than a new `RUN_STARTED` or late `RUN_ERROR` (LIFE-42),
//! nested runs (LIFE-44), unbalanced steps (LIFE-53), triad discipline for
//! text, tool calls and reasoning (TEXT-11, TOOL-37, REAS-08..REAS-13), owner
//! mismatches (SUB-24) and unannounced subagent terminals (SUB-36). It accepts
//! everything legal: unannounced attribution (SUB-12), untagged continuations
//! (SUB-25), interleaving (STRM-05), several runs (LIFE-43).
//!
//! [`ProducerVerifier`] runs in the server runner before the encoder and
//! refuses every producer MUST NOT, so the server never writes an invalid
//! stream (ARCH-04). A refused event leaves its state unchanged, so the runner
//! can report it and continue or fail cleanly.
//!
//! Both are O(1) per event: open sets are hash sets of `Arc<str>` ids.

mod consumer;
mod producer;

pub use consumer::{ConsumerVerifier, RunPhase};
#[cfg(feature = "server")]
pub(crate) use producer::Holder;
pub use producer::{ProducerRule, ProducerVerifier, ProducerViolation};
