//! Linearity: work grows with the frame, not faster.
//!
//! The deterministic check bounds the bytes lookahead re-reads within the
//! byte walk; a reroute or lenient retry adds at most one more linear pass on
//! the `Value` path, which this check does not count. The timing bench is
//! `#[ignore]`d:
//! run it with `cargo test --release -- --ignored enforce_linearity_bench`.

use std::time::Instant;

use serde::de::IgnoredAny;

use super::super::report::{Findings, Options};
use super::super::shape::{event_union, Shape};
use super::super::stream::{Rooted, Stream};

/// A snapshot of `n` messages, with discriminators placed late on purpose
/// (the worst case for lookahead) and text-heavy content.
fn snapshot(n: usize) -> String {
    let mut s = String::from(r#"{"messages":["#);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            r#"{{"id":"m{i}","name":"someone","content":[{{"text":"{}","type":"text"}},{{"source":{{"value":"https://example.com/{i}","type":"url"}},"type":"image"}}],"role":"user"}}"#,
            "lorem ipsum dolor sit amet ".repeat(8)
        ));
    }
    s.push_str(r#"],"type":"MESSAGES_SNAPSHOT"}"#);
    s
}

fn enforce(doc: &str) -> usize {
    let opts = Options::lenient(true);
    let mut findings = Findings::new(8);
    let mut st = Stream::new(doc.as_bytes(), &opts, &mut findings);
    let out = st.run::<IgnoredAny>(Shape::Union(event_union()));
    assert!(matches!(out, Ok(Rooted::Value(_))));
    st.rescanned
}

#[test]
fn lookahead_rereads_a_bounded_share_of_the_frame() {
    let mut ratios = Vec::new();
    for n in [100, 200, 400, 800] {
        let doc = snapshot(n);
        let rescanned = enforce(&doc);
        // Each union level can look ahead over its own object once; the
        // schema nests at most four (event, message, part, source).
        assert!(
            rescanned <= 4 * doc.len(),
            "{rescanned} re-read of {}",
            doc.len()
        );
        ratios.push(rescanned as f64 / doc.len() as f64);
    }
    // The re-read share is flat across sizes: linear, not quadratic.
    let (lo, hi) = ratios
        .iter()
        .fold((f64::MAX, 0.0f64), |(lo, hi), r| (lo.min(*r), hi.max(*r)));
    assert!(hi - lo < 0.01, "{ratios:?}");
}

#[test]
#[ignore = "timing bench: cargo test --release -- --ignored enforce_linearity_bench"]
fn enforce_linearity_bench() {
    let base = 250;
    let mut per = Vec::new();
    for scale in [1, 2, 4, 8] {
        let doc = snapshot(base * scale);
        let mut samples: Vec<f64> = (0..15)
            .map(|_| {
                let t = Instant::now();
                enforce(&doc);
                t.elapsed().as_secs_f64()
            })
            .collect();
        samples.sort_by(|a, b| a.total_cmp(b));
        let median = samples[samples.len() / 2];
        eprintln!(
            "x{scale}: {} bytes, median {:.3} ms, {:.0} MB/s",
            doc.len(),
            median * 1e3,
            doc.len() as f64 / median / 1e6
        );
        per.push(median);
    }
    let ratio = per[3] / per[0];
    assert!(ratio <= 8.0 * 1.15, "x8/x1 = {ratio:.2}");
}
