//! Version declaration and classification (`basic/versioning.mdx`, "Version
//! negotiation"): the `version` helpers every side uses to read a peer's
//! `protocolVersion` and to decide whether to declare its own.
//!
//! The client-surface halves of these rows (`ClientBuilder::peer_version`)
//! are added to this file.

use agui_sdk::diagnostics::{DiagnosticCode, Scope};
use agui_sdk::version::{LegacyEra, PeerPin, PeerVersion, ProtocolVersion};
use agui_sdk::PROTOCOL_VERSION;

/// covers: VER-17
#[test]
fn declaration_is_the_current_version_unless_a_pre_versioning_pin_applies() {
    // What a client declares parses back as exactly the version it speaks.
    assert_eq!(
        ProtocolVersion::parse(PROTOCOL_VERSION),
        Some(ProtocolVersion::CURRENT)
    );
    assert_eq!(ProtocolVersion::CURRENT.to_string(), PROTOCOL_VERSION);
    // A peer pinned to an era that predates the field is recognised, so the
    // declaration can be withheld from it (fixture
    // era-pinned-peer-omits-protocol-version pins "0.0.57").
    assert_eq!(LegacyEra::parse("0.0.57"), Some(LegacyEra::V0_0_57));
    // A current peer is not mistaken for one, so it gets the declaration
    // (fixture current-peer-declares-protocol-version).
    assert_eq!(LegacyEra::parse(PROTOCOL_VERSION), None);
    assert_eq!(LegacyEra::parse("0.0.58"), None);
}

/// covers: VER-19
#[test]
fn absent_declaration_is_a_pre_versioning_peer_and_quiet() {
    let peer = PeerVersion::classify(None);
    assert_eq!(peer, PeerVersion::Absent);
    assert_eq!(peer.diagnostic_code(), None);
    assert_eq!(peer.diagnostic(Scope::Event), None);
    // Absent is not the same as an empty declaration, which is unreadable.
    assert_eq!(
        PeerVersion::classify(Some("")),
        PeerVersion::Uninterpretable(String::new())
    );
}

/// covers: VER-20
#[test]
fn versions_compare_numerically_per_component() {
    let v = |text| ProtocolVersion::parse(text);
    assert!(v("1.10") > v("1.9"));
    assert!(v("2.0") > v("1.99"));
    assert!(v("1.2") > v("1.1"));
    assert_eq!(v("1.10"), Some(ProtocolVersion::new(1, 10)));
    assert_eq!(
        PeerVersion::classify(Some("1.10")),
        PeerVersion::NewerMinor(ProtocolVersion::new(1, 10))
    );
    // Only MAJOR.MINOR as published: no third component, no padding.
    for text in [
        "1", "1.", ".0", "1.0.0", "v1.0", " 1.0", "1.0 ", "01.0", "1.00", "+1.0", "1.-1",
    ] {
        assert_eq!(v(text), None, "{text:?}");
    }
    assert_eq!(v("18446744073709551616.0"), None, "overflow is unreadable");
    assert_eq!(v("0.0"), Some(ProtocolVersion::new(0, 0)));
}

/// covers: VER-21
#[test]
fn uninterpretable_declaration_proceeds_with_a_warning() {
    let peer = PeerVersion::classify(Some("draft"));
    assert_eq!(peer, PeerVersion::Uninterpretable("draft".to_owned()));
    assert_eq!(
        peer.diagnostic_code(),
        Some(DiagnosticCode::UninterpretableVersion)
    );
    let warning = peer.diagnostic(Scope::Event);
    let Some(warning) = warning else {
        panic!("an uninterpretable declaration must warn");
    };
    assert_eq!(warning.code, DiagnosticCode::UninterpretableVersion);
    assert_eq!(warning.pointer, "/protocolVersion");
    // Wording pinned by producer-declares-uninterpretable-version-warns.
    assert!(
        warning.message.contains("cannot interpret"),
        "{}",
        warning.message
    );
    assert!(
        !warning.message.contains("draft"),
        "peer text is not quoted"
    );
    for text in ["1.0.0", "latest", "1.x"] {
        assert!(matches!(
            PeerVersion::classify(Some(text)),
            PeerVersion::Uninterpretable(_)
        ));
    }
}

/// covers: VER-26
#[test]
fn newer_minor_producer_declaration_proceeds_with_a_warning() {
    let peer = PeerVersion::classify(Some("1.1"));
    assert_eq!(peer, PeerVersion::NewerMinor(ProtocolVersion::new(1, 1)));
    assert_eq!(
        peer.diagnostic_code(),
        Some(DiagnosticCode::NewerMinorVersion)
    );
    let Some(warning) = peer.diagnostic(Scope::Event) else {
        panic!("a newer minor must warn");
    };
    assert_eq!(warning.scope, Scope::Event);
    // Wording pinned by producer-declares-newer-version-warns.
    assert!(warning.message.contains("1.1"), "{}", warning.message);
    assert!(warning.message.contains("Upgrade"), "{}", warning.message);
    // A different major line also proceeds on the client, and warns.
    let other = PeerVersion::classify(Some("2.0"));
    assert_eq!(
        other,
        PeerVersion::UnsupportedMajor(ProtocolVersion::new(2, 0))
    );
    assert_eq!(
        other.diagnostic_code(),
        Some(DiagnosticCode::UnsupportedMajorVersion)
    );
    assert!(other.diagnostic(Scope::Event).is_some());
}

/// covers: VER-27
#[test]
fn same_declared_version_is_silent() {
    let peer = PeerVersion::classify(Some(PROTOCOL_VERSION));
    assert_eq!(peer, PeerVersion::Same);
    assert_eq!(peer.diagnostic_code(), None);
    assert_eq!(peer.diagnostic(Scope::Event), None);
    // An older minor of the same line has nothing to warn about either.
    // (There is none below 1.0 today; 1.0 is the first frozen version.)
    assert_eq!(
        PeerVersion::OlderMinor(ProtocolVersion::new(1, 0)).diagnostic_code(),
        None
    );
}

/// covers: VER-30
#[test]
fn a_known_legacy_peer_selects_its_era_downgrade() {
    let era = |text| LegacyEra::parse(text);
    assert_eq!(era("0.0.0"), Some(LegacyEra::V0_0_39));
    assert_eq!(era("0.0.39"), Some(LegacyEra::V0_0_39));
    assert_eq!(era("0.0.40"), Some(LegacyEra::V0_0_47));
    assert_eq!(era("0.0.47"), Some(LegacyEra::V0_0_47));
    assert_eq!(era("0.0.48"), Some(LegacyEra::V0_0_57));
    assert_eq!(era("0.0.57"), Some(LegacyEra::V0_0_57));
    assert_eq!(era("0.0.58"), None);
    assert_eq!(era("0.0.100"), None);
    // Older eras need every later era's shims too, so they order first.
    assert!(LegacyEra::V0_0_39 < LegacyEra::V0_0_47);
    assert!(LegacyEra::V0_0_47 < LegacyEra::V0_0_57);
    assert_eq!(LegacyEra::V0_0_57.as_str(), "0.0.57");
}

/// covers: VER-31
#[test]
fn unknown_peer_version_is_treated_as_current() {
    // Nothing configured or declared: no era is selected, nothing warns.
    assert_eq!(PeerPin::default(), PeerPin::Current);
    assert_eq!(PeerVersion::classify(None).diagnostic_code(), None);
    for text in [
        "", "unknown", "0.0", "0.0.", "0.0.x", "0.0.039", "0.1.0", "1.0",
    ] {
        assert_eq!(
            LegacyEra::parse(text),
            None,
            "{text:?} must not guess downward"
        );
    }
}

/// Client surface: what `ClientBuilder::peer_version` does on the wire and
/// in diagnostics.
#[cfg(feature = "client")]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod client_pin {
    use std::future::IntoFuture;
    use std::sync::Arc;

    use agui_sdk::client::{AgentClient, ClientBuilder, RunRequest};
    use agui_sdk::content::{ContentPart, MediaPart, PartSource, TextPart, UrlSource};
    use agui_sdk::diagnostics::{CollectingSink, DiagnosticCode};
    use agui_sdk::version::{LegacyEra, PeerPin, ProtocolVersion};
    use agui_test_support::mock::{Delivery, MockTransport};
    use serde_json::{json, Value};

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let std::task::Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
            std::thread::park();
        }
    }

    fn sent(pin: Option<PeerPin>, request: RunRequest) -> (Value, Vec<DiagnosticCode>) {
        let frames = vec![
            json!({"type": "RUN_STARTED", "threadId": "t1", "runId": "r1"}),
            json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": "r1"}),
        ];
        let transport = MockTransport::new(frames, Delivery::Frames);
        let sink = Arc::new(CollectingSink::new());
        let mut builder: ClientBuilder<MockTransport> =
            AgentClient::with_transport(transport.clone()).diagnostic_sink(Arc::clone(&sink));
        if let Some(pin) = pin {
            builder = builder.peer_version(pin);
        }
        let client = builder.build().unwrap();
        block_on(client.run(request).into_future()).unwrap();
        let codes = sink.take().into_iter().map(|d| d.code).collect();
        (transport.last_request().unwrap(), codes)
    }

    fn request() -> RunRequest {
        RunRequest::new().thread_id("t1").run_id("r1")
    }

    /// covers: VER-17
    #[test]
    fn a_client_declares_its_version_on_the_input() {
        let (body, codes) = sent(None, request());
        assert_eq!(body["protocolVersion"], "1.0");
        assert!(codes.is_empty());
    }

    /// covers: VER-19
    #[test]
    fn toward_a_pre_versioning_peer_the_field_is_omitted() {
        for era in [LegacyEra::V0_0_39, LegacyEra::V0_0_47, LegacyEra::V0_0_57] {
            let (body, _) = sent(Some(PeerPin::Legacy(era)), request());
            assert!(body.get("protocolVersion").is_none(), "{era:?}");
        }
    }

    /// covers: VER-30
    #[test]
    fn a_configured_legacy_pin_selects_that_eras_downgrade() {
        let parts = vec![
            ContentPart::Text(TextPart::new("a")),
            ContentPart::Image(MediaPart::new(PartSource::Url(UrlSource::new(
                "https://x/i.png",
            )))),
        ];
        let (body, codes) = sent(
            Some(PeerPin::Legacy(LegacyEra::V0_0_39)),
            request().user(parts.clone()),
        );
        // 0.0.39 reads plain-string content only.
        assert!(body["messages"][0]["content"].is_string(), "{body}");
        assert!(!codes.is_empty(), "the lossy flattening warns");
        let (body, _) = sent(
            Some(PeerPin::Legacy(LegacyEra::V0_0_57)),
            request().user(parts),
        );
        assert!(body["messages"][0]["content"].is_array());
    }

    /// covers: VER-31
    #[test]
    fn an_unknown_peer_is_treated_as_current() {
        let parts = vec![ContentPart::Text(TextPart::new("a"))];
        let (body, codes) = sent(None, request().user(parts));
        assert!(body["messages"][0]["content"].is_array());
        assert_eq!(body["protocolVersion"], "1.0");
        assert!(codes.is_empty());
    }

    /// covers: VER-26
    #[test]
    fn a_pin_to_a_newer_minor_proceeds_and_warns_once() {
        let (body, codes) = sent(
            Some(PeerPin::Version(ProtocolVersion::new(1, 7))),
            request(),
        );
        assert_eq!(body["protocolVersion"], "1.0");
        assert_eq!(codes, [DiagnosticCode::NewerMinorVersion]);
    }

    /// covers: VER-27
    #[test]
    fn a_pin_to_the_same_version_is_silent() {
        let (_, codes) = sent(
            Some(PeerPin::Version(ProtocolVersion::new(1, 0))),
            request(),
        );
        assert!(codes.is_empty());
    }
}

/// The consumer pipeline judges the producer's `RUN_STARTED.protocolVersion`:
/// a newer minor or an uninterpretable declaration is served with one
/// warning on that event; the same version, an older one or none is quiet.
///
/// covers: VER-21, VER-26, VER-27
#[test]
fn processor_warns_on_a_newer_or_unreadable_producer_declaration() {
    use std::sync::Arc;

    use agui_sdk::diagnostics::{CollectingSink, Reporter};
    use agui_sdk::pipeline::{PipelineConfig, Processor};
    use agui_sdk::EventType;

    let warnings = |declared: Option<&str>| {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink.clone(), 8);
        let mut processor = Processor::new(PipelineConfig::default());
        let mut started = serde_json::json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"});
        if let Some(v) = declared {
            started["protocolVersion"] = v.into();
        }
        let mut out = Vec::new();
        let frame = serde_json::to_vec(&started).unwrap_or_default();
        assert!(processor.push_json(&frame, &mut reporter, &mut out).is_ok());
        assert_eq!(out.len(), 1, "the run is served");
        sink.take()
    };
    let newer = warnings(Some("1.1"));
    assert_eq!(newer.len(), 1);
    assert_eq!(newer[0].code, DiagnosticCode::NewerMinorVersion);
    assert_eq!(newer[0].event_type, Some(EventType::RunStarted));
    assert!(newer[0].message.contains("1.1"), "{}", newer[0].message);
    let unreadable = warnings(Some("draft"));
    assert_eq!(unreadable.len(), 1);
    assert_eq!(unreadable[0].code, DiagnosticCode::UninterpretableVersion);
    assert!(
        !unreadable[0].message.contains("draft"),
        "peer text is not quoted"
    );
    for quiet in [Some(PROTOCOL_VERSION), Some("1.0"), None] {
        assert!(warnings(quiet).is_empty(), "{quiet:?}");
    }
}
