//! Version parsing over arbitrary text. Invariant: no panic; a parsed
//! version displays as text that parses to the same version.
#![no_main]

use agui_sdk::version::{LegacyEra, PeerVersion, ProtocolVersion};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(v) = ProtocolVersion::parse(text) {
        assert_eq!(ProtocolVersion::parse(&v.to_string()), Some(v));
    }
    let _ = LegacyEra::parse(text);
    let _ = PeerVersion::classify(Some(text)).diagnostic_code();
});
