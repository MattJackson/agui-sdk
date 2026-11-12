//! The protobuf binding, consumer side (`transports/http-protobuf.mdx`):
//! negotiation from the client's end, framing, and decoding into the JSON
//! form every transport hands to the shared pipeline.

#![cfg(feature = "proto")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod proto_support;

use agui_sdk::proto::decode::{decode_event, decode_event_json, Decoded, DecodedJson};
use agui_sdk::proto::encode::encode_json;
use agui_sdk::proto::framing::{write_frame, FrameDecoder, DEFAULT_MAX_FRAME_BYTES};
use std::sync::Arc;

use agui_sdk::diagnostics::{CollectingSink, DiagnosticCode, Reporter};
use agui_sdk::pipeline::enforce::Enforcer;
use agui_sdk::pipeline::Limits;
use agui_sdk::wire::Chunk;
use agui_sdk::wire::{WireFormat, PROTO_MEDIA_TYPE, SSE_MEDIA_TYPE};
use agui_sdk::{ErrorCode, EventType};
use proto_support::{byte_cases, ld, sem_eq, vi};
use serde_json::{json, Value};

/// A handful of events spanning the shapes the codec maps specially.
const SAMPLES: &[&str] = &[
    r#"{"type":"RUN_STARTED","threadId":"t1","runId":"r1","protocolVersion":"1.0"}"#,
    r#"{"type":"TEXT_MESSAGE_START","messageId":"m1","role":"assistant","timestamp":1700000000000}"#,
    r#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"m1","delta":"hello é 😀"}"#,
    r#"{"type":"STATE_DELTA","delta":[{"op":"replace","path":"/a","value":{"b":[1,2.5,null,true]}}]}"#,
    r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"u1","role":"user","content":[{"type":"text","text":"hi"}]},{"id":"a1","role":"assistant","content":"yo"}]}"#,
    r#"{"type":"RUN_FINISHED","threadId":"t1","runId":"r1","outcome":{"type":"success"},"metadata":{"k":"v"}}"#,
    r#"{"type":"RUN_ERROR","message":"model overloaded","code":"overloaded"}"#,
];

fn encode(json: &str) -> Vec<u8> {
    let mut out = Vec::new();
    encode_json(json.as_bytes(), &mut out).unwrap();
    out
}

fn decode(bytes: &[u8]) -> Value {
    match decode_event(bytes).unwrap() {
        Decoded::Event(v) => v,
        other => panic!("not an event: {other:?}"),
    }
}

fn stream(events: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in events {
        write_frame(&encode(e), &mut out).unwrap();
    }
    out
}

/// Feeds `body` in `size`-byte chunks and returns the frames, plus whether
/// bytes of an incomplete frame were left at the end.
fn frames(body: &[u8], size: usize) -> (Vec<Chunk>, bool) {
    let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
    let mut got = Vec::new();
    for c in body.chunks(size) {
        d.push(Chunk::copy_from_slice(c));
        while let Some(f) = d.next_frame().unwrap() {
            got.push(f);
        }
    }
    (got, d.has_partial())
}

/// covers: PB-01
#[test]
fn opting_in_admits_protobuf_alongside_sse() {
    let accept = WireFormat::client_accept(true);
    assert!(accept.contains(PROTO_MEDIA_TYPE), "{accept}");
    assert!(accept.contains(SSE_MEDIA_TYPE), "{accept}");
    // And what the client then reads decodes.
    let (got, partial) = frames(&stream(SAMPLES), 4096);
    assert!(!partial);
    assert_eq!(got.len(), SAMPLES.len());
}

/// covers: PB-02
#[test]
fn a_client_without_protobuf_names_only_sse_explicitly() {
    let accept = WireFormat::client_accept(false);
    assert!(accept.contains(SSE_MEDIA_TYPE));
    assert!(!accept.contains(PROTO_MEDIA_TYPE));
    assert!(!accept.contains('*'), "no wildcard: {accept}");
}

/// covers: PB-06
#[test]
fn a_protobuf_client_still_recognises_an_sse_answer() {
    assert_eq!(
        WireFormat::from_content_type("text/event-stream; charset=utf-8"),
        Some(WireFormat::Sse)
    );
    assert_eq!(
        WireFormat::negotiate(Some(WireFormat::client_accept(true)), false),
        WireFormat::Sse,
        "a producer without protobuf answers SSE to a protobuf-capable client"
    );
}

/// covers: PB-07
#[test]
fn the_parser_is_selected_by_the_exact_content_type() {
    assert_eq!(
        agui_sdk::proto::MEDIA_TYPE,
        "application/vnd.ag-ui.event+proto"
    );
    assert_eq!(
        WireFormat::from_content_type(PROTO_MEDIA_TYPE),
        Some(WireFormat::Protobuf)
    );
    assert_eq!(
        WireFormat::from_content_type("application/x-protobuf"),
        None
    );
}

/// covers: PB-08
#[test]
fn a_frame_is_a_big_endian_length_then_exactly_one_event() {
    let event = encode(SAMPLES[2]);
    let mut out = Vec::new();
    write_frame(&event, &mut out).unwrap();
    assert_eq!(out[..4], (event.len() as u32).to_be_bytes());
    assert_eq!(out[4..], event[..]);
    // Two frames abut with no separator.
    let body = stream(&SAMPLES[..2]);
    let first = encode(SAMPLES[0]);
    assert_eq!(
        body[4 + first.len()..8 + first.len()],
        (encode(SAMPLES[1]).len() as u32).to_be_bytes()
    );
    let (got, partial) = frames(&body, body.len());
    assert!(!partial);
    assert_eq!(decode(&got[0])["type"], "RUN_STARTED");
    assert_eq!(decode(&got[1])["type"], "TEXT_MESSAGE_START");
    // An empty payload is a legal frame; whether it decodes is the
    // decoder's question (it does not: PB-18).
    let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
    d.push(Chunk::from_static(&[0, 0, 0, 0]));
    assert_eq!(d.next_frame().unwrap().as_deref(), Some(&[][..]));
}

/// covers: PB-09
#[test]
fn frames_split_across_chunks_and_several_per_chunk_decode_identically() {
    let body = stream(SAMPLES);
    let (whole, _) = frames(&body, body.len());
    assert_eq!(whole.len(), SAMPLES.len());
    for size in [1, 2, 3, 4, 5, 7, 13, 64, 1000] {
        let (got, partial) = frames(&body, size);
        assert!(!partial, "chunk {size}");
        assert_eq!(got, whole, "chunk {size}");
    }
    // The whole corpus, framed and re-chunked.
    if let Some(cases) = byte_cases() {
        let mut body = Vec::new();
        for c in &cases {
            write_frame(&c.bin, &mut body).unwrap();
        }
        for size in [1, 3, 4, 5, 17, 4096] {
            let (got, partial) = frames(&body, size);
            assert!(!partial);
            let want: Vec<&[u8]> = cases.iter().map(|c| &c.bin[..]).collect();
            let got: Vec<&[u8]> = got.iter().map(|b| &b[..]).collect();
            assert_eq!(got, want, "chunk {size}");
        }
    }
}

/// covers: PB-10
#[test]
fn a_body_ending_mid_frame_is_truncation_not_a_decode_failure() {
    let body = stream(SAMPLES);
    for cut in [1, 3, 4, 5, body.len() - 1] {
        let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
        d.push(Chunk::copy_from_slice(&body[..cut]));
        let mut n = 0;
        while let Some(f) = d.next_frame().unwrap() {
            decode_event(&f).unwrap();
            n += 1;
        }
        assert!(n < SAMPLES.len());
        assert!(d.has_partial(), "cut at {cut}: truncated, not clean");
    }
    let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
    d.push(Chunk::from(body));
    while d.next_frame().unwrap().is_some() {}
    assert!(!d.has_partial(), "a complete body is a clean end");
}

/// The size limit is enforced from the header alone, before the payload
/// is buffered.
#[test]
fn an_oversized_frame_is_refused_at_its_header() {
    assert_eq!(DEFAULT_MAX_FRAME_BYTES, 16 * 1024 * 1024);
    let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
    d.push(Chunk::copy_from_slice(&((16 << 20) + 1u32).to_be_bytes()));
    assert_eq!(d.next_frame().unwrap_err().code(), ErrorCode::FrameTooLarge);
    // Exactly at the limit is fine (and waits for the payload).
    let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
    d.push(Chunk::copy_from_slice(&(16u32 << 20).to_be_bytes()));
    assert_eq!(d.next_frame().unwrap(), None);
    // A header split across chunks is checked once complete.
    let mut d = FrameDecoder::new(8);
    d.push(Chunk::from_static(&[0, 0]));
    assert_eq!(d.next_frame().unwrap(), None);
    d.push(Chunk::from_static(&[0, 9, 1, 2]));
    assert_eq!(d.next_frame().unwrap_err().code(), ErrorCode::FrameTooLarge);
    assert_eq!(
        FrameDecoder::new(0).next_frame().unwrap(),
        None,
        "nothing buffered is not an error"
    );
}

/// covers: PB-11
#[test]
fn every_event_type_of_the_schema_has_a_wire_arm() {
    for t in EventType::ALL {
        let name = serde_json::to_value(t).unwrap();
        let name = name.as_str().unwrap();
        let mut out = Vec::new();
        encode_json(format!(r#"{{"type":"{name}"}}"#).as_bytes(), &mut out)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(decode(&out)["type"], name);
    }
}

/// covers: PB-13
#[test]
fn decoded_events_are_the_same_json_an_sse_data_field_carries() {
    for src in SAMPLES {
        let bytes = encode(src);
        let mut text = String::new();
        let kind = decode_event_json(&bytes, &mut text).unwrap();
        let want: Value = serde_json::from_str(src).unwrap();
        assert!(matches!(kind, DecodedJson::Event(t) if want["type"] == t));
        // Plain JSON text, ready for `Processor::push_json`, as SSE gives.
        let got: Value = serde_json::from_str(&text).unwrap();
        assert!(sem_eq(&want, &got), "{src}\n{text}");
        assert_eq!(decode(&bytes), got);
    }
}

/// covers: PB-14
#[test]
fn unrecognised_open_material_survives_decoding() {
    let src = r#"{"type":"RUN_FINISHED","threadId":"t","runId":"r","outcome":{"type":"teleported","interruptIds":["i1"]},"metadata":{"x-new":{"deep":[1]}},"rawEvent":{"vendor":"?"}}"#;
    let got = decode(&encode(src));
    assert_eq!(got["outcome"]["type"], "teleported");
    assert_eq!(got["metadata"], json!({"x-new": {"deep": [1]}}));
    assert_eq!(got["rawEvent"], json!({"vendor": "?"}));
    let src = r#"{"type":"SUBAGENT_FINISHED","subagentRunId":"s","outcome":{"type":"evaporated"}}"#;
    assert_eq!(decode(&encode(src))["outcome"]["type"], "evaporated");
}

/// covers: PB-15
#[test]
fn fields_the_wire_schema_predates_are_skipped_silently() {
    // TEXT_MESSAGE_START (arm 1): base_event{type 0} + messageId, plus unknown
    // fields of every wire type at every level.
    let mut unknown = vi(99, 5);
    unknown.extend(ld(98, b"zz"));
    unknown.extend([0x0d, 1, 2, 3, 4]); // field 1 as fixed32: wrong type, skipped
    unknown.extend([0xf1, 0x06, 1, 2, 3, 4, 5, 6, 7, 8]); // field 110, fixed64
    let base = [vi(1, 0), unknown.clone()].concat();
    let body = [ld(1, &base), ld(2, b"m1"), unknown.clone()].concat();
    let bytes = [ld(1, &body), unknown].concat();
    assert_eq!(
        decode(&bytes),
        json!({"type":"TEXT_MESSAGE_START","messageId":"m1"})
    );
}

/// covers: PB-16
#[test]
fn patch_operation_extension_members_do_not_survive_protobuf() {
    use agui_sdk::pipeline::{PipelineConfig, Processor};

    let with =
        r#"{"type":"STATE_DELTA","delta":[{"op":"add","path":"/a","value":1,"x-trace":"t"}]}"#;
    let without = r#"{"type":"STATE_DELTA","delta":[{"op":"add","path":"/a","value":1}]}"#;
    assert_eq!(encode(with), encode(without));
    assert_eq!(
        decode(&encode(with))["delta"][0],
        json!({"op":"add","path":"/a","value":1})
    );

    // Through the consumer pipeline: the JSON wire delivers the extension
    // member to the application; the protobuf wire cannot carry it.
    let started = br#"{"type":"RUN_STARTED","threadId":"t","runId":"r"}"#;
    let delivered = |json_wire: bool| -> Value {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink.clone(), 256);
        let mut processor = Processor::new(PipelineConfig::default());
        let mut out = Vec::new();
        processor
            .push_json(started, &mut reporter, &mut out)
            .unwrap();
        if json_wire {
            processor
                .push_json(with.as_bytes(), &mut reporter, &mut out)
                .unwrap();
        } else {
            processor
                .push_value(decode(&encode(with)), &mut reporter, &mut out)
                .unwrap();
        }
        assert!(sink.take().is_empty(), "nothing here is unrecognised");
        assert_eq!(processor.reducer().state(), &json!({"a": 1}));
        serde_json::to_value(&*out[1].event).unwrap()
    };
    assert_eq!(
        delivered(true)["delta"][0],
        json!({"op":"add","path":"/a","value":1,"x-trace":"t"})
    );
    assert!(delivered(false)["delta"][0].get("x-trace").is_none());

    // An undefined `from` (on an op other than `move`/`copy`) that is not a
    // string is an extension member too: not carried, never an encode
    // failure, on the text, value and typed paths alike.
    for from in ["5", "true", "{}", "[1]", "null"] {
        let odd = format!(
            r#"{{"type":"STATE_DELTA","delta":[{{"op":"remove","path":"/a","from":{from}}}]}}"#
        );
        let plain = r#"{"type":"STATE_DELTA","delta":[{"op":"remove","path":"/a"}]}"#;
        assert_eq!(encode(&odd), encode(plain), "{from}");
        let value: Value = serde_json::from_str(&odd).unwrap();
        let mut out = Vec::new();
        agui_sdk::proto::encode::encode_value(&value, &mut out).unwrap();
        assert_eq!(out, encode(plain), "{from}");
        let typed: agui_sdk::Event = serde_json::from_value(value).unwrap();
        let mut out = Vec::new();
        agui_sdk::proto::encode::encode_event(&typed, &mut out).unwrap();
        assert_eq!(out, encode(plain), "{from}");
    }
    // A string `from` on any op still travels as field 3, and a non-string
    // one on `move`/`copy` is still malformed.
    let strung = r#"{"type":"STATE_DELTA","delta":[{"op":"remove","path":"/a","from":"/x"}]}"#;
    assert_eq!(decode(&encode(strung))["delta"][0]["from"], "/x");
    for op in ["move", "copy"] {
        let bad = json!({"type":"STATE_DELTA","delta":[{"op": op,"path":"/a","from":5}]});
        let mut out = Vec::new();
        assert!(
            agui_sdk::proto::encode::encode_value(&bad, &mut out).is_err(),
            "{op}"
        );
    }
}

/// covers: PB-17
#[test]
fn an_envelope_arm_this_build_predates_is_reported_for_dropping() {
    // The decoder half. The warning is spelled by the HTTP transport, and
    // `transport_http.rs` drives it (`unknown_protobuf_arms_are_dropped_with_a_warning`).
    let bytes = ld(64, &ld(1, b""));
    assert_eq!(
        decode_event(&bytes).unwrap(),
        Decoded::UnknownArm { field_number: 64 }
    );
    let mut text = String::from("kept");
    assert_eq!(
        decode_event_json(&bytes, &mut text).unwrap(),
        DecodedJson::UnknownArm { field_number: 64 }
    );
    assert_eq!(text, "kept", "nothing written");
    // A known arm beside an unknown one wins.
    let bytes = [ld(40, b""), ld(3, &ld(2, b"m"))].concat();
    assert!(matches!(decode_event(&bytes).unwrap(), Decoded::Event(_)));
}

/// covers: PROC-08
#[test]
fn an_unknown_envelope_variant_is_not_fatal() {
    // Even in mid-stream, between known frames.
    let mut body = stream(&SAMPLES[..1]);
    write_frame(&ld(77, &ld(1, &vi(1, 99))), &mut body).unwrap();
    write_frame(&encode(SAMPLES[1]), &mut body).unwrap();
    let (got, partial) = frames(&body, 3);
    assert!(!partial);
    let verdicts: Vec<Decoded> = got.iter().map(|f| decode_event(f).unwrap()).collect();
    assert!(matches!(verdicts[0], Decoded::Event(_)));
    assert_eq!(verdicts[1], Decoded::UnknownArm { field_number: 77 });
    assert!(matches!(verdicts[2], Decoded::Event(_)));
}

/// covers: PB-18
#[test]
fn an_envelope_with_no_arm_at_all_is_fatal() {
    // A newer arm still carries its tag; an envelope with none maps to no
    // event at all, a decode failure like `{}` on the JSON wire.
    assert_eq!(
        decode_event(&[]).unwrap_err().code(),
        ErrorCode::ProtoFraming
    );
    let mut text = String::from("kept");
    assert_eq!(
        decode_event_json(&[], &mut text).unwrap_err().code(),
        ErrorCode::ProtoFraming
    );
    assert_eq!(text, "kept", "nothing written");
}

/// covers: PB-18
#[test]
fn bytes_that_do_not_decode_as_a_message_are_fatal() {
    let good = encode(SAMPLES[2]);
    for n in 1..good.len() {
        let err = decode_event(&good[..n]).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ProtoFraming, "prefix {n}");
        let mut text = String::from("kept");
        assert!(decode_event_json(&good[..n], &mut text).is_err());
        assert_eq!(text, "kept", "output restored on error");
    }
    for bad in [
        ld(3, &ld(2, &[0xff, 0xfe])), // invalid UTF-8
        vec![0x00],                   // field number 0
        vec![0x0e],                   // wire type 6
        vec![
            0x08, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01,
        ], // overlong varint
    ] {
        assert_eq!(
            decode_event(&bad).unwrap_err().code(),
            ErrorCode::ProtoFraming,
            "{bad:02x?}"
        );
    }
}

/// covers: PB-19
#[test]
fn an_in_stream_failure_is_a_run_error_frame_like_any_other() {
    let body = stream(&[SAMPLES[0], SAMPLES[6]]);
    let (got, partial) = frames(&body, 5);
    assert!(!partial);
    assert_eq!(
        decode(&got[1]),
        json!({"type":"RUN_ERROR","message":"model overloaded","code":"overloaded"})
    );
}

/// covers: PROC-06
#[test]
fn an_event_reaches_the_pipeline_identically_over_either_wire() {
    for src in SAMPLES {
        let json_wire: Value = serde_json::from_str(src).unwrap();
        let proto_wire = decode(&encode(src));
        assert!(sem_eq(&json_wire, &proto_wire), "{src}\n{proto_wire}");
    }
    if let Some(cases) = byte_cases() {
        for c in cases {
            let json_wire: Value = serde_json::from_slice(&c.json).unwrap();
            assert!(sem_eq(&json_wire, &decode(&c.bin)), "{}", c.name);
        }
    }
}

/// covers: PROC-09
#[test]
fn absent_required_strings_and_lists_materialise_empty() {
    // TEXT_MESSAGE_CONTENT with no messageId and no delta on the wire.
    let bytes = ld(2, &ld(1, &vi(1, 1)));
    assert_eq!(
        decode(&bytes),
        json!({"type":"TEXT_MESSAGE_CONTENT","messageId":"","delta":""})
    );
    // STATE_DELTA with no operations: the required list is [].
    assert_eq!(
        decode(&ld(8, &ld(1, &vi(1, 7)))),
        json!({"type":"STATE_DELTA","delta":[]})
    );
    // Optional members stay absent, never null.
    let v = decode(&encode(
        r#"{"type":"TEXT_MESSAGE_START","messageId":"m1","role":null}"#,
    ));
    assert_eq!(v, json!({"type":"TEXT_MESSAGE_START","messageId":"m1"}));
}

/// A STATE_DELTA whose first operation is an `op` this build predates, as the
/// binary wire carries it: `JsonPatchOperation{op: 9, path: "/count",
/// value: 1}` then a `replace`.
fn delta_with_unknown_op() -> Vec<u8> {
    let number = |x: f64| [vec![0x11], x.to_le_bytes().to_vec()].concat();
    let unknown = [vi(1, 9), ld(2, b"/count"), ld(4, &number(1.0))].concat();
    let replace = [vi(1, 2), ld(2, b"/count"), ld(4, &number(5.0))].concat();
    let body = [ld(1, &vi(1, 7)), ld(2, &unknown), ld(2, &replace)].concat();
    ld(8, &body)
}

/// Enforces one JSON frame: the kept event as JSON (`None`: dropped), and
/// the warnings as (code, pointer).
fn enforce(frame: &[u8]) -> (Option<Value>, Vec<(DiagnosticCode, String)>) {
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 256);
    let kept = Enforcer::new(Limits::default())
        .enforce_event_json(frame, &mut reporter)
        .unwrap()
        .map(|e| serde_json::to_value(e).unwrap());
    let warnings = sink
        .take()
        .into_iter()
        .map(|d| (d.code, d.pointer))
        .collect();
    (kept, warnings)
}

#[test]
fn an_unknown_enum_number_decodes_as_a_spelling_no_value_uses() {
    let bytes = delta_with_unknown_op();
    let v = decode(&bytes);
    assert_eq!(
        v,
        json!({"type":"STATE_DELTA","delta":[
            {"op":"#9","path":"/count","value":1},
            {"op":"replace","path":"/count","value":5}]})
    );
    // Both decoders agree, and the frame re-encodes unchanged.
    let mut text = String::new();
    decode_event_json(&bytes, &mut text).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), v);
    assert_eq!(encode(&text), bytes);
    // A negative number is spelled with its sign.
    let neg = ld(
        8,
        &[
            ld(1, &vi(1, 7)),
            ld(2, &[vi(1, u64::MAX), ld(2, b"/a")].concat()),
        ]
        .concat(),
    );
    assert_eq!(decode(&neg)["delta"][0]["op"], "#-1");
    // `#n` naming a known value, or anything else, is still refused on encode.
    for op in ["#2", "#x", "increment"] {
        let src = format!(r#"{{"type":"STATE_DELTA","delta":[{{"op":"{op}","path":"/a"}}]}}"#);
        let err = encode_json(src.as_bytes(), &mut Vec::new()).unwrap_err();
        assert_eq!(err.code(), ErrorCode::MalformedEvent, "{op}");
    }
}

/// covers: PROC-06
#[test]
fn an_unknown_patch_op_gets_the_same_verdict_over_either_wire() {
    // The JSON wire (fixture state-delta-unknown-op-dropped).
    let json_wire = br#"{"type":"STATE_DELTA","delta":[{"op":"increment","path":"/count","value":1},{"op":"replace","path":"/count","value":5}]}"#;
    // The protobuf wire: the same event with an op number this build predates.
    let mut proto_wire = String::new();
    decode_event_json(&delta_with_unknown_op(), &mut proto_wire).unwrap();

    let (json_kept, json_warnings) = enforce(json_wire);
    let (proto_kept, proto_warnings) = enforce(proto_wire.as_bytes());
    let want = json!({"type":"STATE_DELTA","delta":[{"op":"replace","path":"/count","value":5}]});
    assert_eq!(json_kept, Some(want.clone()));
    assert_eq!(proto_kept, Some(want));
    let dropped = vec![(DiagnosticCode::UnknownPatchOp, "/delta/0".to_owned())];
    assert_eq!(json_warnings, dropped);
    assert_eq!(proto_warnings, dropped);
}

/// proto-7: an `int64` beyond the safe range reaches the pipeline as the exact
/// integer, not rounded through `f64`, so enforcement judges the same number
/// it would see over SSE.
#[test]
fn an_int64_beyond_the_safe_range_is_handed_on_exactly() {
    const BIG: u64 = 9_007_199_254_740_993;
    // TEXT_MESSAGE_START{base_event{timestamp: BIG}, message_id: "m"}.
    let bytes = ld(1, &[ld(1, &vi(2, BIG)), ld(2, b"m")].concat());
    let tree = decode(&bytes);
    assert_eq!(tree["timestamp"], json!(BIG));
    let mut text = String::new();
    decode_event_json(&bytes, &mut text).unwrap();
    assert!(text.contains(r#""timestamp":9007199254740993"#), "{text}");
    // The enforcement verdict equals the one for the same number as SSE text.
    let verdict = |frame: &[u8]| {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink, 256);
        Enforcer::new(Limits::default())
            .enforce_event_json(frame, &mut reporter)
            .map(|e| e.map(|e| serde_json::to_value(e).unwrap()))
            .map_err(|e| (e.code(), e.to_string()))
    };
    let sse = br#"{"type":"TEXT_MESSAGE_START","timestamp":9007199254740993,"messageId":"m"}"#;
    let want = verdict(sse);
    assert_eq!(want.as_ref().unwrap_err().0, ErrorCode::InvalidValue);
    assert_eq!(verdict(text.as_bytes()), want);
}

/// proto-9: a payload beyond what the `u32` wire can describe is a frame-size
/// failure, not an encode failure. The zeroed allocation is never touched.
#[cfg(target_pointer_width = "64")]
#[test]
fn a_decode_input_beyond_four_gib_is_reported_as_too_large() {
    let big = vec![0u8; (u32::MAX as usize) + 1];
    let err = decode_event(&big).unwrap_err();
    assert_eq!(err.code(), ErrorCode::FrameTooLarge);
}
