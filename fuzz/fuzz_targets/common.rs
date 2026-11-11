//! Shared helpers for the fuzz targets (included with `#[path]`).

#![allow(dead_code)]

use agui_sdk::wire::Chunk;

/// Splits `data` into a chunk size (first byte, 1..=256) and the payload.
pub fn chunking(data: &[u8]) -> (usize, &[u8]) {
    match data.split_first() {
        Some((first, rest)) => (usize::from(*first) + 1, rest),
        None => (1, data),
    }
}

/// `payload` as `Bytes` chunks of `size`.
pub fn chunks(payload: &[u8], size: usize) -> Vec<Chunk> {
    payload
        .chunks(size.max(1))
        .map(Chunk::copy_from_slice)
        .collect()
}

/// Every event round-trips through the strict parser: what the pipeline hands
/// out is post-enforcement.
pub fn assert_strict(event: &agui_sdk::Event) {
    let mut out = Vec::new();
    event.write_json(&mut out);
    if let Err(e) = agui_sdk::Event::parse_strict(&out) {
        panic!(
            "pipeline produced a non-strict event: {e}: {}",
            String::from_utf8_lossy(&out)
        );
    }
}
