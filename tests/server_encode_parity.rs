//! Byte parity of the server's protobuf encoder (PB-12, decision D4): every
//! event case of the TypeScript byte corpus, read into a typed `Event` and
//! written by `server::encode::Encoder` as a frame. With `preserve-order` the
//! frame's payload is the corpus `.bin` byte for byte; in every build it is
//! byte-identical to the JSON-text path over the event's own JSON. (Decoding
//! is the protobuf lane's business, `corpus_proto_bytes.rs`.)

#![cfg(all(feature = "server", feature = "proto"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agui_sdk::proto::encode::encode_json;
use agui_sdk::server::encode::Encoder;
use agui_sdk::wire::WireFormat;
use agui_sdk::Event;
use agui_test_support::corpus;
use agui_test_support::lane::check_each;

#[test]
fn the_server_protobuf_encoder_reproduces_the_byte_corpus() {
    let Some(cases) = corpus::proto_cases() else {
        return;
    };
    check_each(
        "server/proto-parity",
        &cases,
        |c| c.name.clone(),
        |c| {
            let event: Event = serde_json::from_slice(&c.json).map_err(|e| e.to_string())?;
            let mut frame = Vec::new();
            Encoder::for_format(WireFormat::Protobuf)
                .encode(&event, &mut frame)
                .map_err(|e| e.to_string())?;
            let (head, payload) = frame.split_at(4);
            if u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize != payload.len() {
                return Err("length prefix does not match the payload".into());
            }
            // The JSON-text path over the typed event's own JSON.
            let mut text_path = Vec::new();
            encode_json(&serde_json::to_vec(&event).unwrap(), &mut text_path)
                .map_err(|e| e.to_string())?;
            if payload != text_path.as_slice() {
                return Err("frame differs from the JSON-text path".into());
            }
            if cfg!(feature = "preserve-order") && payload != c.bytes.as_slice() {
                return Err(format!(
                    "typed path is not byte-identical to the corpus ({} vs {} bytes)",
                    payload.len(),
                    c.bytes.len()
                ));
            }
            Ok(None)
        },
    );
}
