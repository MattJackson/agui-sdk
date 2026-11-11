//! Stream lane: every upstream conformance fixture (spec/1.0/conformance/
//! streams, discovered from the spec cache) runs four ways, and each way must
//! satisfy the fixture's `expect` block as resolved for the Rust lane
//! (`tests/corpus/rust-lane-overrides.toml`):
//!
//! 1. frames straight into `Processor::push_json`;
//! 2. SSE through `MockTransport` and `AgentClient`, split every 1, 7 and 4096 bytes;
//! 3. protobuf through `MockTransport`, split every 1 and 4096 bytes, for
//!    every fixture whose events protobuf can carry. A fixture with an event
//!    the encoder refuses is skipped with the encoder's reason (computed);
//!    a fixture whose defect the encoder would silently lose is skipped by
//!    name, with the rule that makes it inexpressible ([`PROTO_INEXPRESSIBLE`]);
//! 4. `Processor::push_value`, the path middleware forces.
//!
//! TEST-PLAN §3. The lane assertions are the oracle's; this file only drives.

use agui_test_support::corpus::{self, StreamFixture};
use agui_test_support::lane::check_each_skipping;
use agui_test_support::mock::{proto_expressible, Delivery};
use agui_test_support::oracle::{self, JudgeOptions, Observed, Way};
use agui_test_support::overrides::Overrides;

/// Fixtures whose defect the binary wire cannot carry, although every event
/// encodes: the encoder would deliver a different stream, so the JSON
/// verdict is not comparable (PROC-06 asks for the same verdict only where
/// the wire can express the difference). Each entry names its reason; an
/// entry whose fixture is gone, or no longer encodes, fails the lane.
const PROTO_INEXPRESSIBLE: &[(&str, &str)] = &[
    (
        "subagent-started-missing-name-fatal",
        "SUBAGENT_STARTED.name is an implicit-presence proto3 string: an absent name \
         and an empty one are the same bytes, so the decoder materialises \"\" and the \
         stream is accepted where JSON rejects the missing field (PROC-09)",
    ),
    (
        "unknown-property-stripped",
        "the undescribed property `futureProp` has no field number, so it cannot be \
         written on the wire at all; a newer producer's unknown field number is skipped \
         silently by the decoder (PB-15), never named as a JSON path",
    ),
];

/// Everything the protobuf lanes skip, pinned by name: the two
/// [`PROTO_INEXPRESSIBLE`] entries, then the six fixtures with an event the
/// encoder refuses (computed by `proto_expressible`, listed here so that a
/// regression which makes the encoder refuse more cannot pass as skips).
/// Exactly these 8 are skipped; one more, or one fewer, fails the lane.
const PROTO_SKIPS: [&str; 8] = [
    "subagent-started-missing-name-fatal",
    "unknown-property-stripped",
    "era-0-0-45-thinking-translated",
    "malformed-known-field-fatal",
    "state-delta-unknown-op-dropped",
    "unknown-event-dropped",
    "unknown-outcome-array-fatal",
    "unknown-part-in-message-list",
];

/// The JSON, value and SSE lanes run every fixture.
const NO_SKIPS: &[&str] = &[];

fn lane(
    name: &str,
    observe: impl Fn(&StreamFixture) -> Result<Observed, String>,
    options: JudgeOptions,
    expected_skips: &[&str],
) {
    let Some(fixtures) = corpus::streams() else {
        return;
    };
    let overrides = Overrides::load();
    if let Err(stale) = overrides.check_against(&fixtures) {
        panic!("{}", stale.join("\n"));
    }
    check_each_skipping(
        name,
        &fixtures,
        |f| f.name.clone(),
        |f| {
            let observed = match observe(f) {
                Ok(observed) => observed,
                Err(skip) => return Ok(Some(skip)),
            };
            oracle::judge_with(f, &observed, &overrides, &options)
                .map(|_| None)
                .map_err(|p| p.join("\n    "))
        },
        expected_skips,
    );
}

fn proto(f: &StreamFixture, chunk: usize) -> Result<Observed, String> {
    let named = PROTO_INEXPRESSIBLE
        .iter()
        .find(|(fixture, _)| *fixture == f.name);
    match (proto_expressible(f.stream()), named) {
        (Err(why), None) => Err(format!("protobuf cannot carry this stream: {why}")),
        (Err(why), Some(_)) => panic!(
            "PROTO_INEXPRESSIBLE names {}, which the encoder already refuses ({why}): \
             delete the entry",
            f.name
        ),
        (Ok(()), Some((_, reason))) => Err(format!("protobuf cannot express the defect: {reason}")),
        (Ok(()), None) => Ok(oracle::observe_client_blocking(
            f,
            Delivery::Protobuf { chunk },
        )),
    }
}

fn proto_lane(name: &str, chunk: usize) {
    if let Some(fixtures) = corpus::streams() {
        let stale: Vec<&str> = PROTO_INEXPRESSIBLE
            .iter()
            .map(|(fixture, _)| *fixture)
            .filter(|fixture| !fixtures.iter().any(|f| f.name == *fixture))
            .collect();
        assert!(
            stale.is_empty(),
            "PROTO_INEXPRESSIBLE names fixtures the corpus no longer has: {stale:?}"
        );
    }
    lane(name, |f| proto(f, chunk), JudgeOptions::new(), &PROTO_SKIPS);
}

#[test]
fn frames_push_json() {
    lane(
        "frames/json",
        |f| Ok(oracle::observe(f, Way::Json)),
        JudgeOptions::frames(),
        NO_SKIPS,
    );
}

#[test]
fn frames_push_value() {
    lane(
        "frames/value",
        |f| Ok(oracle::observe(f, Way::Value)),
        JudgeOptions::frames(),
        NO_SKIPS,
    );
}

#[test]
fn sse_split_every_byte() {
    lane(
        "sse/1",
        |f| {
            Ok(oracle::observe_client_blocking(
                f,
                Delivery::Sse { chunk: 1 },
            ))
        },
        JudgeOptions::new(),
        NO_SKIPS,
    );
}

#[test]
fn sse_split_every_7_bytes() {
    lane(
        "sse/7",
        |f| {
            Ok(oracle::observe_client_blocking(
                f,
                Delivery::Sse { chunk: 7 },
            ))
        },
        JudgeOptions::new(),
        NO_SKIPS,
    );
}

#[test]
fn sse_split_every_4096_bytes() {
    lane(
        "sse/4096",
        |f| {
            Ok(oracle::observe_client_blocking(
                f,
                Delivery::Sse { chunk: 4096 },
            ))
        },
        JudgeOptions::new(),
        NO_SKIPS,
    );
}

#[test]
fn protobuf_split_every_byte() {
    proto_lane("proto/1", 1);
}

#[test]
fn protobuf_split_every_4096_bytes() {
    proto_lane("proto/4096", 4096);
}
