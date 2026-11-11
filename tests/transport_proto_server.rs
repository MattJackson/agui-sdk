//! The protobuf binding, producer side: negotiation, the media type, framing
//! and encoding, including byte parity with the canonical corpus.

#![cfg(feature = "proto")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod proto_support;
#[cfg(feature = "server")]
mod support_server;

use agui_sdk::proto::decode::{decode_event, decode_event_json, Decoded};
use agui_sdk::proto::encode::{encode_event, encode_json, encode_value};
use agui_sdk::proto::framing::{write_frame, FrameDecoder, DEFAULT_MAX_FRAME_BYTES};
use agui_sdk::wire::Chunk;
use agui_sdk::wire::{WireFormat, PROTO_MEDIA_TYPE};
use agui_sdk::{ErrorCode, Event, EventType};
use proto_support::byte_cases;
use serde_json::{json, Value};

fn encode(json: &str) -> agui_sdk::Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_json(json.as_bytes(), &mut out).map(|()| out)
}

fn decode(bytes: &[u8]) -> Value {
    match decode_event(bytes).unwrap() {
        Decoded::Event(v) => v,
        other => panic!("not an event: {other:?}"),
    }
}

fn text(bytes: &[u8]) -> String {
    let mut s = String::new();
    decode_event_json(bytes, &mut s).unwrap();
    s
}

/// covers: PB-03
#[test]
fn relative_quality_values_are_not_consulted() {
    for accept in [
        "text/event-stream;q=1, application/vnd.ag-ui.event+proto;q=0.1",
        "application/vnd.ag-ui.event+proto;q=0.001, text/event-stream",
    ] {
        assert_eq!(
            WireFormat::negotiate(Some(accept), true),
            WireFormat::Protobuf,
            "{accept}"
        );
    }
}

/// covers: PB-04
#[test]
fn a_producer_with_protobuf_answers_with_it_whenever_admitted() {
    for accept in [
        PROTO_MEDIA_TYPE,
        "text/event-stream, application/vnd.ag-ui.event+proto",
        "*/*",
        "application/*",
    ] {
        assert_eq!(
            WireFormat::negotiate(Some(accept), true),
            WireFormat::Protobuf,
            "{accept}"
        );
    }
}

/// covers: PB-05
#[test]
fn otherwise_the_producer_answers_sse() {
    for accept in [
        Some("text/event-stream"),
        Some("text/event-stream, application/vnd.ag-ui.event+proto;q=0"),
        None,
    ] {
        assert_eq!(
            WireFormat::negotiate(accept, true),
            WireFormat::Sse,
            "{accept:?}"
        );
    }
    assert_eq!(
        WireFormat::negotiate(Some(PROTO_MEDIA_TYPE), false),
        WireFormat::Sse,
        "a producer without protobuf ignores the type"
    );
}

/// The shipped HTTP handler, asked for protobuf: the response
/// `Content-Type` and the whole encoded body, or the pre-stream rejection.
#[cfg(feature = "server")]
fn serve_protobuf<A: agui_sdk::server::Agent>(
    runner: &agui_sdk::server::Runner<A>,
    body: &[u8],
) -> Result<(&'static str, Vec<u8>), agui_sdk::server::http::HttpRejection> {
    use agui_sdk::server::http::{handle, HttpRequest, ServeConfig};
    let request = HttpRequest::new(Some("application/json"), Some(PROTO_MEDIA_TYPE), body);
    let response = handle(
        runner,
        &ServeConfig::default(),
        request,
        agui_sdk::server::CancelToken::new(),
    )?;
    let mut stream = response.body;
    let mut bytes = Vec::new();
    futures_executor::block_on(async {
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk);
        }
    });
    Ok((response.content_type, bytes))
}

/// Splits a body by the binding's framing, read independently of the SDK's
/// decoder: a 4-byte big-endian length, then exactly that many bytes, with
/// nothing between frames; each payload decoded.
#[cfg(feature = "server")]
fn unframe(body: &[u8]) -> Vec<Value> {
    let mut events = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        assert!(rest.len() >= 4, "a partial length prefix: {rest:?}");
        let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        assert!(rest.len() >= 4 + len, "a frame runs past the body");
        events.push(decode(&rest[4..4 + len]));
        rest = &rest[4 + len..];
    }
    events
}

#[cfg(feature = "server")]
fn input_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "threadId": "t1", "runId": "r1", "protocolVersion": "1.0",
        "messages": [{"id": "u1", "role": "user", "content": "hi"}]
    }))
    .unwrap()
}

#[cfg(feature = "server")]
fn stepping_agent(fail: bool) -> agui_sdk::server::Runner<impl agui_sdk::server::Agent> {
    agui_sdk::server::Runner::new(support_server::agent(move |ctx| async move {
        let step = ctx.emitter().start_step("plan").await?;
        step.finish().await?;
        if fail {
            return Err("the model went away".into());
        }
        Ok(agui_sdk::server::RunOutcome::success())
    }))
}

/// covers: PB-07
#[test]
fn the_response_content_type_is_exactly_the_protobuf_media_type() {
    assert_eq!(
        WireFormat::Protobuf.media_type(),
        "application/vnd.ag-ui.event+proto"
    );
    assert_eq!(agui_sdk::proto::MEDIA_TYPE, PROTO_MEDIA_TYPE);
    // What the shipped handler answers with when protobuf is negotiated.
    #[cfg(feature = "server")]
    {
        let (content_type, _) = serve_protobuf(&stepping_agent(false), &input_body()).unwrap();
        assert_eq!(content_type, "application/vnd.ag-ui.event+proto");
    }
}

/// covers: PB-08
#[test]
fn encoded_events_are_framed_with_a_big_endian_length_and_no_separator() {
    let a = encode(r#"{"type":"STEP_STARTED","stepName":"plan"}"#).unwrap();
    let b = encode(r#"{"type":"STEP_FINISHED","stepName":"plan"}"#).unwrap();
    let mut body = Vec::new();
    write_frame(&a, &mut body).unwrap();
    write_frame(&b, &mut body).unwrap();
    let mut want = (a.len() as u32).to_be_bytes().to_vec();
    want.extend_from_slice(&a);
    want.extend_from_slice(&(b.len() as u32).to_be_bytes());
    want.extend_from_slice(&b);
    assert_eq!(body, want);
    let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
    d.push(Chunk::from(body));
    assert_eq!(d.next_frame().unwrap().as_deref(), Some(&a[..]));
    assert_eq!(d.next_frame().unwrap().as_deref(), Some(&b[..]));
    // The body the shipped handler streams is framed the same way.
    #[cfg(feature = "server")]
    {
        let (_, body) = serve_protobuf(&stepping_agent(false), &input_body()).unwrap();
        let types: Vec<Value> = unframe(&body)
            .into_iter()
            .map(|e| e["type"].clone())
            .collect();
        assert_eq!(
            types,
            [
                "RUN_STARTED",
                "STEP_STARTED",
                "STEP_FINISHED",
                "RUN_FINISHED"
            ]
        );
    }
}

/// covers: PB-11
#[test]
fn the_encoder_covers_every_event_type_of_the_schema() {
    for t in EventType::ALL {
        let name = serde_json::to_value(t).unwrap();
        let name = name.as_str().unwrap().to_owned();
        let bytes = encode(&format!(r#"{{"type":"{name}"}}"#)).unwrap();
        assert_eq!(decode(&bytes)["type"], name.as_str());
    }
    let err = encode(r#"{"type":"NOT_A_TYPE"}"#).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedEvent);
}

/// covers: PB-12
#[test]
fn the_text_encoder_reproduces_the_canonical_corpus_byte_for_byte() {
    let Some(cases) = byte_cases() else {
        return;
    };
    let mut out = Vec::new();
    for c in &cases {
        out.clear();
        encode_json(&c.json, &mut out).unwrap();
        assert_eq!(out, c.bin, "{}", c.name);
        // With preserve-order the typed and `Value` encoders do too (D4).
        if cfg!(feature = "preserve-order") {
            let v: Value = serde_json::from_slice(&c.json).unwrap();
            out.clear();
            encode_value(&v, &mut out).unwrap();
            assert_eq!(out, c.bin, "{}: encode_value", c.name);
        }
    }
}

/// covers: PB-19
#[test]
fn a_failure_inside_the_stream_is_encoded_as_a_run_error_frame() {
    let event: Event =
        serde_json::from_value(json!({"type":"RUN_ERROR","message":"boom","code":"agent.failed"}))
            .unwrap();
    let mut payload = Vec::new();
    encode_event(&event, &mut payload).unwrap();
    let mut body = Vec::new();
    write_frame(&payload, &mut body).unwrap();
    let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
    d.push(Chunk::from(body));
    let frame = d.next_frame().unwrap().unwrap();
    assert_eq!(
        decode(&frame),
        json!({"type":"RUN_ERROR","message":"boom","code":"agent.failed"})
    );
    #[cfg(feature = "server")]
    {
        // Through the shipped handler: an in-stream failure ends the
        // protobuf body with a RUN_ERROR frame, after what was already sent.
        let (_, body) = serve_protobuf(&stepping_agent(true), &input_body()).unwrap();
        let events = unframe(&body);
        let types: Vec<&Value> = events.iter().map(|e| &e["type"]).collect();
        assert_eq!(
            types,
            ["RUN_STARTED", "STEP_STARTED", "STEP_FINISHED", "RUN_ERROR"]
        );
        // A pre-stream rejection is an HTTP error status, with no stream.
        let refused = serve_protobuf(&stepping_agent(false), b"{not json").unwrap_err();
        assert_eq!(refused.status, 400);
    }
}

#[test]
fn struct_entries_follow_javascript_property_order() {
    // Index keys ascending first, then insertion order; a repeated key keeps
    // its first position and its last value.
    let a =
        encode(r#"{"type":"CUSTOM","name":"n","value":{"b":1,"2":2,"a":3,"10":4,"01":5,"b":6}}"#)
            .unwrap();
    let b = encode(r#"{"type":"CUSTOM","name":"n","value":{"2":2,"10":4,"b":6,"a":3,"01":5}}"#)
        .unwrap();
    assert_eq!(a, b);
    assert_eq!(
        text(&a),
        r#"{"type":"CUSTOM","name":"n","value":{"2":2,"10":4,"b":6,"a":3,"01":5}}"#
    );
    // The value path agrees on index keys whatever the map order.
    let v: Value =
        serde_json::from_str(r#"{"type":"CUSTOM","name":"n","value":{"10":1,"9":2}}"#).unwrap();
    let mut out = Vec::new();
    encode_value(&v, &mut out).unwrap();
    assert_eq!(
        text(&out),
        r#"{"type":"CUSTOM","name":"n","value":{"9":2,"10":1}}"#
    );
}

#[test]
fn value_maps_encode_in_insertion_order_only_with_preserve_order() {
    let v: Value =
        serde_json::from_str(r#"{"type":"CUSTOM","name":"n","value":{"z":1,"a":2}}"#).unwrap();
    let mut out = Vec::new();
    encode_value(&v, &mut out).unwrap();
    let want = if cfg!(feature = "preserve-order") {
        r#"{"type":"CUSTOM","name":"n","value":{"z":1,"a":2}}"#
    } else {
        r#"{"type":"CUSTOM","name":"n","value":{"a":2,"z":1}}"#
    };
    assert_eq!(text(&out), want);
    // The text path keeps the text's order in every build.
    let from_text = encode(r#"{"type":"CUSTOM","name":"n","value":{"z":1,"a":2}}"#).unwrap();
    assert!(text(&from_text).ends_with(r#""value":{"z":1,"a":2}}"#));
}

#[test]
fn numbers_are_javascript_numbers() {
    let b =
        encode(r#"{"type":"CUSTOM","name":"n","value":[1.5,1e21,-0,42,1e-7,123.456e2]}"#).unwrap();
    assert_eq!(
        text(&b),
        r#"{"type":"CUSTOM","name":"n","value":[1.5,1e+21,0,42,1e-7,12345.6]}"#
    );
    // int64 fields accept only integers in the safe range.
    let err = encode(r#"{"type":"TEXT_MESSAGE_END","messageId":"m","timestamp":9007199254740992}"#)
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Encode);
    assert!(encode(r#"{"type":"TEXT_MESSAGE_END","messageId":"m","timestamp":1.5}"#).is_err());
    assert!(encode(r#"{"type":"TEXT_MESSAGE_END","messageId":"m","timestamp":1e3}"#).is_ok());
}

#[test]
fn null_typed_members_are_absent_and_open_nulls_survive() {
    let plain = encode(r#"{"type":"TEXT_MESSAGE_START","messageId":"m1"}"#).unwrap();
    let nulls = encode(
        r#"{"type":"TEXT_MESSAGE_START","messageId":"m1","role":null,"metadata":null,"timestamp":null}"#,
    )
    .unwrap();
    assert_eq!(plain, nulls);
    let open =
        encode(r#"{"type":"TEXT_MESSAGE_START","messageId":"m1","metadata":{"x":null}}"#).unwrap();
    assert_eq!(decode(&open)["metadata"], json!({"x": null}));
}

#[test]
fn encode_errors_carry_codes_and_leave_the_buffer_untouched() {
    let cases: [(&str, ErrorCode); 6] = [
        ("[]", ErrorCode::MalformedEvent),
        ("{}", ErrorCode::MalformedEvent),
        (
            r#"{"type":"TEXT_MESSAGE_START","messageId":5}"#,
            ErrorCode::MalformedEvent,
        ),
        (
            r#"{"type":"STATE_DELTA","delta":[{"op":"frobnicate","path":""}]}"#,
            ErrorCode::MalformedEvent,
        ),
        (r#"{"type":"TEXT_MESSAGE_START",}"#, ErrorCode::Json),
        (
            r#"{"type":"CUSTOM","name":"n","value":"\ud800"}"#,
            ErrorCode::Encode,
        ),
    ];
    for (src, code) in &cases[..5] {
        let mut out = b"prefix".to_vec();
        let err = encode_json(src.as_bytes(), &mut out).unwrap_err();
        assert_eq!(err.code(), *code, "{src}");
        assert_eq!(out, b"prefix", "{src}");
    }
    // A lone surrogate is not an error: it becomes U+FFFD, as TextEncoder does.
    let (src, _) = cases[5];
    assert_eq!(decode(&encode(src).unwrap())["value"], "\u{fffd}");
    let mut out = Vec::new();
    assert_eq!(
        encode_json(&[b'"', 0xff, b'"'], &mut out)
            .unwrap_err()
            .code(),
        ErrorCode::Json
    );
}

#[test]
fn nesting_is_bounded_in_both_directions() {
    let deep = |n: usize| {
        format!(
            r#"{{"type":"CUSTOM","name":"n","value":{}1{}}}"#,
            "[".repeat(n),
            "]".repeat(n)
        )
    };
    // The encoder takes up to 511 levels; the decoder, facing the peer,
    // only what enforcement accepts (127 arrays inside the event object).
    let ok = encode(&deep(511)).unwrap();
    assert_eq!(
        decode_event(&ok).unwrap_err().code(),
        ErrorCode::LimitExceeded
    );
    let ok = encode(&deep(127)).unwrap();
    assert!(matches!(decode_event(&ok), Ok(Decoded::Event(_))));
    assert_eq!(
        decode_event(&encode(&deep(128)).unwrap())
            .unwrap_err()
            .code(),
        ErrorCode::LimitExceeded
    );
    assert_eq!(
        encode(&deep(512)).unwrap_err().code(),
        ErrorCode::LimitExceeded
    );
    // 5000 levels of nested ListValue on the wire: an error, not a crash.
    let mut inner = vec![0x11, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f];
    for _ in 0..5000 {
        inner = proto_support::ld(6, &proto_support::ld(1, &inner));
    }
    let bytes = proto_support::ld(
        11,
        &[
            proto_support::ld(1, &proto_support::vi(1, 10)),
            proto_support::ld(2, b"n"),
            proto_support::ld(3, &inner),
        ]
        .concat(),
    );
    assert!(decode_event(&bytes).is_err());
}

#[test]
fn content_shapes_round_trip() {
    for src in [
        r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"1","role":"user","content":[{"type":"text","text":"look"},{"type":"image","source":{"type":"data","value":"aGk=","mimeType":"image/png"},"metadata":{"w":1}},{"type":"audio","source":{"type":"url","value":"https://x"}}]}]}"#,
        r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"6","role":"activity","activityType":"a","content":{"k":[1,"two",null,true,{"n":{}}]}}]}"#,
        r#"{"type":"ACTIVITY_SNAPSHOT","messageId":"a","activityType":"t","content":{},"replace":false,"timestamp":0,"rawEvent":null}"#,
    ] {
        let want: Value = serde_json::from_str(src).unwrap();
        let bytes = encode(src).unwrap();
        assert_eq!(decode(&bytes), want, "{src}");
        let mut again = Vec::new();
        encode_value(&want, &mut again).unwrap();
        assert_eq!(again, bytes, "{src}");
    }
}

#[test]
fn encoders_append_to_the_callers_buffer() {
    let mut out = b"head".to_vec();
    encode_json(br#"{"type":"STEP_STARTED","stepName":"s"}"#, &mut out).unwrap();
    assert!(out.starts_with(b"head"));
    assert_eq!(
        decode(&out[4..]),
        json!({"type":"STEP_STARTED","stepName":"s"})
    );
}
