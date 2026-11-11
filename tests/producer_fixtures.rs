//! Producer lane: this repository's fixtures under `tests/fixtures/producer/`
//! (TEST-PLAN §4). Consumer fixtures are never replayed here.
//!
//! * A producer-legal stream passes `ProducerVerifier` end to end, and the
//!   consumer pipeline too.
//! * A must-refuse case is refused exactly at `at`, with the named rule, and
//!   the refusal leaves the verifier's state unchanged: the rest of the
//!   stream after the refused event is still accepted.

use std::sync::Arc;

use agui_sdk::diagnostics::{CollectingSink, Reporter};
use agui_sdk::pipeline::verify::ProducerVerifier;
use agui_sdk::pipeline::{PipelineConfig, Processor, Termination};
use agui_sdk::Event;
use agui_test_support::lane::check_each;
use agui_test_support::producer::{self, ProducerFixture};

fn parse(f: &ProducerFixture) -> Result<Vec<Event>, String> {
    f.events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            serde_json::from_value(e.clone())
                .map_err(|err| format!("event {i} does not parse: {err}"))
        })
        .collect()
}

#[test]
fn producer_fixtures() {
    let fixtures = producer::fixtures();
    check_each(
        "producer",
        &fixtures,
        |f| f.name.clone(),
        |f| {
            let events = parse(f)?;
            let mut verifier = ProducerVerifier::new();
            match &f.refuse {
                None => {
                    for (i, e) in events.iter().enumerate() {
                        verifier
                            .check(e)
                            .map_err(|v| format!("event {i} refused: {v:?}"))?;
                    }
                    let mut processor = Processor::new(PipelineConfig::default());
                    let sink = Arc::new(CollectingSink::new());
                    let mut reporter = Reporter::new(sink, 256);
                    let mut out = Vec::new();
                    for (i, e) in f.events.iter().enumerate() {
                        let bytes = serde_json::to_vec(e).map_err(|err| err.to_string())?;
                        processor
                            .push_json(&bytes, &mut reporter, &mut out)
                            .map_err(|err| {
                                format!("consumer pipeline rejected event {i}: {err}")
                            })?;
                    }
                    // The consumer must also see a complete run: a legal
                    // producer stream ends with its terminal.
                    match processor.finish_checked() {
                        Ok(Termination::Complete) => {}
                        Ok(other) => {
                            return Err(format!("consumer pipeline: the stream ends {other:?}"))
                        }
                        Err(err) => return Err(format!("consumer pipeline at the end: {err}")),
                    }
                    Ok(None)
                }
                Some((rule, at)) => {
                    for (i, e) in events.iter().enumerate() {
                        match (verifier.check(e), i == *at) {
                            (Ok(()), false) => {}
                            (Err(v), true) if format!("{:?}", v.rule) == *rule => {}
                            (Err(v), true) => {
                                return Err(format!(
                                    "event {i} refused as {:?}, expected {rule}",
                                    v.rule
                                ))
                            }
                            (Ok(()), true) => {
                                return Err(format!("event {i} accepted, expected refusal {rule}"))
                            }
                            (Err(v), false) => {
                                return Err(format!(
                                    "event {i} refused ({v:?}); only {at} should be"
                                ))
                            }
                        }
                    }
                    Ok(None)
                }
            }
        },
    );
}
