//! Every `HttpTransportBuilder` option against a local HTTP/1.1 mock server:
//! headers and their reservation, redaction, redirects, the four timeouts,
//! retry, bounded error bodies, content-type selection, cancellation, proxies,
//! TLS roots and client identities, and capability fetches.

mod transport_support;

use std::time::{Duration, Instant};

use agui_sdk::client::capabilities::CapabilitiesRequest;
use agui_sdk::client::transport::{
    Header, HttpLimits, HttpTransport, HttpTransportBuilder, RetryPolicy, Transport,
};
use agui_sdk::ErrorCode;
use serde_json::json;
use transport_support::*;

const MS: Duration = Duration::from_millis(1);

fn build(builder: HttpTransportBuilder) -> HttpTransport {
    builder.build().unwrap_or_else(|e| panic!("{e}"))
}

fn ok_reply() -> Reply {
    Reply::sse(sse(&events()))
}

// ---------------------------------------------------------------- headers

#[test]
fn sends_transport_and_per_run_headers() {
    block(async {
        let server = MockServer::always(ok_reply())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(
            HttpTransport::builder(server.url("/"))
                .header("X-Tenant", "acme")
                .sensitive_header("X-Api-Key", "k-123")
                .bearer_token("tok"),
        );
        let mut req = request();
        req.headers = vec![
            Header::new("X-Trace", "abc"),
            Header::new("X-Tenant", "beta"),
            Header::new("Authorization", "Bearer tenant"),
            Header::new("X-Multi", "one"),
            Header::new("X-Multi", "two"),
        ];
        let (frames, _) = drain(transport.open(req).await.unwrap_or_else(|e| panic!("{e}"))).await;
        assert_eq!(frames, events());
        let got = server.last_request().unwrap_or_default();
        assert_eq!(
            got.header_all("authorization"),
            ["Bearer tenant"],
            "a per-run Authorization replaces the shared one"
        );
        assert_eq!(got.header("x-api-key"), Some("k-123"));
        assert_eq!(got.header("x-trace"), Some("abc"));
        assert_eq!(got.header_all("x-tenant"), ["beta"], "replaced, not added");
        assert_eq!(
            got.header_all("x-multi"),
            ["one", "two"],
            "several per-run values of one name are all sent"
        );
    });
}

#[test]
fn reserved_headers_cannot_be_overridden() {
    for name in [
        "Accept",
        "content-type",
        "Host",
        "Content-Length",
        "Transfer-Encoding",
        "connection",
    ] {
        let built = HttpTransport::builder("http://127.0.0.1:1/")
            .header(name, "x")
            .build();
        expect_code(built, ErrorCode::InvalidConfig);
        let per_run = block(async move {
            let server = MockServer::always(ok_reply())
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let transport = build(HttpTransport::builder(server.url("/")));
            let mut req = request();
            req.headers = vec![Header::new(name, "*/*")];
            let result = transport.open(req).await;
            (result.map(|_| ()), server.requests().len())
        });
        expect_code(per_run.0, ErrorCode::InvalidRequest);
        assert_eq!(per_run.1, 0, "nothing sent for {name}");
    }
    let bad = HttpTransport::builder("http://h/")
        .header("X-Ok", "a\r\nInjected: 1")
        .build();
    expect_code(bad, ErrorCode::InvalidConfig);
    let bad = HttpTransport::builder("http://h/")
        .header("Bad Name", "a")
        .build();
    expect_code(bad, ErrorCode::InvalidConfig);
}

#[test]
fn url_credentials_become_basic_auth() {
    block(async {
        let server = MockServer::always(ok_reply())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let url = format!("http://user%40x:p%3Ass%FF@{}/run?k=v#frag", server.addr());
        let transport = build(HttpTransport::builder(url));
        let _ = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        let got = server.last_request().unwrap_or_default();
        let expected = format!(
            "Basic {}",
            agui_test_support::http_mock::base64(b"user@x:p:ss\xff")
        );
        assert_eq!(got.header("authorization"), Some(expected.as_str()));
        assert_eq!(
            got.target, "/run?k=v",
            "no userinfo or fragment on the wire"
        );
    });
}

#[test]
fn debug_redacts_every_secret() {
    let builder = HttpTransport::builder("https://user:hunter2@agent.example/run?token=qwerty#f")
        .bearer_token("bearer-secret")
        .sensitive_header("X-Custom", "custom-secret")
        .header("X-Api-Key", "apikey-secret")
        .header("Cookie", "cookie-secret")
        .header("User-Agent", "visible-value")
        .proxy("http://puser:proxy-secret@proxy.example:3128")
        .client_identity_pem(b"chain".to_vec(), b"key-secret".to_vec());
    let text = format!("{builder:?}");
    for secret in [
        "hunter2",
        "qwerty",
        "bearer-secret",
        "custom-secret",
        "apikey-secret",
        "cookie-secret",
        "proxy-secret",
        "key-secret",
    ] {
        assert!(
            !text.contains(secret),
            "builder Debug leaks {secret}: {text}"
        );
    }
    assert!(text.contains("visible-value"));
    assert!(text.contains("agent.example/run"));
    // A password with an unencoded `/` is redacted too (before `build`
    // would reject it).
    let text = format!("{:?}", HttpTransport::builder("http://svc:pa/ss@h/"));
    assert!(!text.contains("ss@") && !text.contains("pa/"), "{text}");
    if cfg!(any(feature = "rustls", feature = "native-tls")) {
        let transport = build(
            HttpTransport::builder("https://user:hunter2@agent.example/run?token=qwerty")
                .bearer_token("bearer-secret")
                .header("X-Api-Key", "apikey-secret")
                .proxy("http://puser:proxy-secret@proxy.example:3128"),
        );
        let text = format!("{transport:?}");
        for secret in [
            "hunter2",
            "qwerty",
            "bearer-secret",
            "apikey-secret",
            "proxy-secret",
        ] {
            assert!(
                !text.contains(secret),
                "transport Debug leaks {secret}: {text}"
            );
        }
        assert!(text.contains("response_timeout: 60s"), "{text}");
        assert!(text.contains("connect_timeout: 30s"), "{text}");
    }
    let header = format!("{:?}", Header::sensitive("X-A", "s3cret"));
    assert!(!header.contains("s3cret"));
    let req = format!("{:?}", {
        let mut r = request();
        r.headers = vec![Header::new("Authorization", "Bearer zzz")];
        r
    });
    assert!(!req.contains("zzz"), "{req}");
}

#[test]
fn invalid_urls_are_refused_at_build() {
    for url in [
        "",
        "agent.example/run",
        "ftp://h/",
        "http://",
        "http://[::1",
    ] {
        expect_code(
            HttpTransport::builder(url).build(),
            ErrorCode::InvalidConfig,
        );
    }
    expect_code(
        HttpTransport::builder("http://h/")
            .proxy("socks5://p:1")
            .build(),
        ErrorCode::InvalidConfig,
    );
    expect_code(
        HttpTransport::builder("http://h/")
            .proxy("https://p:1")
            .build(),
        ErrorCode::InvalidConfig,
    );
}

#[cfg(not(any(feature = "rustls", feature = "native-tls")))]
#[test]
fn https_needs_a_tls_feature() {
    let error = expect_code(
        HttpTransport::builder("https://agent.example/").build(),
        ErrorCode::InvalidConfig,
    );
    assert!(error.message().contains("rustls"), "{error}");
    expect_code(
        HttpTransport::builder("http://h/")
            .add_root_certificate_der(vec![1, 2, 3])
            .build(),
        ErrorCode::InvalidConfig,
    );
}

// ---------------------------------------------------------------- redirects

/// A server redirecting every request to `location` with `status`, then a
/// second server (or the same origin) serving the stream.
fn redirect(status: u16, location: String) -> Reply {
    Reply::status(status)
        .header("Location", location)
        .framing(Framing::Length)
}

#[test]
fn redirects_are_not_followed_by_default() {
    block(async {
        let server = MockServer::sequence(vec![redirect(307, "/elsewhere".into()), ok_reply()])
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let result = build(HttpTransport::builder(server.url("/run")))
            .open(request())
            .await;
        let error = expect_code(result, ErrorCode::HttpStatus);
        assert_eq!(error.http_status(), Some(307));
        assert_eq!(server.requests().len(), 1);
    });
}

#[test]
fn same_origin_307_and_308_are_followed_when_enabled() {
    for status in [307u16, 308] {
        block(async move {
            let server = MockServer::start(move |req| {
                if req.target == "/run" {
                    redirect(status, "/moved?x=1".into())
                } else {
                    ok_reply()
                }
            })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
            let transport = build(
                HttpTransport::builder(server.url("/run"))
                    .same_origin_redirects(2)
                    .bearer_token("tok"),
            );
            let (frames, error) = drain(
                transport
                    .open(request())
                    .await
                    .unwrap_or_else(|e| panic!("{e}")),
            )
            .await;
            assert!(error.is_none());
            assert_eq!(frames, events());
            let requests = server.requests();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[1].target, "/moved?x=1");
            assert_eq!(requests[1].method, "POST");
            assert_eq!(requests[1].json(), Some(input()), "the body is re-sent");
            assert_eq!(requests[1].header("authorization"), Some("Bearer tok"));
        });
    }
}

#[test]
fn cross_origin_redirects_are_never_followed() {
    block(async {
        let other = MockServer::always(ok_reply())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let server = MockServer::always(redirect(307, other.url("/steal")))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(
            HttpTransport::builder(server.url("/run"))
                .same_origin_redirects(5)
                .bearer_token("secret"),
        );
        let error = expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert_eq!(error.http_status(), Some(307));
        assert!(
            other.requests().is_empty(),
            "credentials never left the origin"
        );
        // Same host and scheme, different port: a different origin.
        let same_host = format!("http://127.0.0.1:{}/x", other.addr().port());
        let server = MockServer::always(redirect(308, same_host))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/run")).same_origin_redirects(5));
        expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert!(other.requests().is_empty());
    });
}

#[test]
fn method_changing_redirects_and_loops_stop() {
    block(async {
        for status in [301u16, 302, 303] {
            let server = MockServer::sequence(vec![redirect(status, "/b".into()), ok_reply()])
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let transport =
                build(HttpTransport::builder(server.url("/a")).same_origin_redirects(3));
            let error = expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
            assert_eq!(error.http_status(), Some(status));
            assert_eq!(server.requests().len(), 1);
        }
        let server = MockServer::always(redirect(307, "/loop".into()))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/a")).same_origin_redirects(3));
        expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert_eq!(
            server.requests().len(),
            4,
            "the first request plus three redirects"
        );
    });
}

// ---------------------------------------------------------------- timeouts

#[test]
fn response_header_timeout() {
    block(async {
        let server = MockServer::always(ok_reply().head_delay(Duration::from_secs(5)))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")).response_timeout(150 * MS));
        let started = Instant::now();
        let error = expect_code(transport.open(request()).await, ErrorCode::Timeout);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(error.message().contains("response"), "{error}");
        assert!(!error.is_retryable());
    });
}

#[test]
fn a_disabled_response_timeout_does_not_overflow_the_clock() {
    block(async {
        let server = MockServer::always(ok_reply())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport =
            build(HttpTransport::builder(server.url("/")).response_timeout(Duration::MAX));
        let (frames, _) = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert_eq!(frames, events());
    });
}

#[test]
fn a_disabled_error_body_timeout_does_not_overflow_the_clock() {
    block(async {
        let server = MockServer::always(Reply::status(400).body("x"))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let mut limits = HttpLimits::default();
        limits.error_body_timeout = Duration::MAX;
        let transport = build(HttpTransport::builder(server.url("/")).limits(limits));
        expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
    });
}

#[test]
fn a_huge_backoff_past_the_total_timeout_reports_the_status_without_overflowing() {
    for jitter in [true, false] {
        block(async {
            let server = MockServer::sequence(vec![Reply::status(503).body("x"), ok_reply()])
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let mut policy = RetryPolicy::exponential(1);
            policy.initial_backoff = Duration::MAX;
            policy.max_backoff = Duration::MAX;
            policy.jitter = jitter;
            let transport = build(
                HttpTransport::builder(server.url("/"))
                    .retry(policy)
                    .total_timeout(Duration::from_secs(5)),
            );
            let started = Instant::now();
            // The wait would pass the deadline: the 503 is reported.
            let error = expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
            assert_eq!(error.http_status(), Some(503));
            assert!(started.elapsed() < Duration::from_secs(3));
        });
    }
}

#[test]
fn idle_timeout_cuts_silence_but_not_a_long_healthy_stream() {
    block(async {
        let body = sse(&events());
        // Silence longer than the idle limit after the first bytes.
        let server =
            MockServer::always(Reply::sse(body.clone()).pieces(40, Duration::from_millis(400)))
                .await
                .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")).idle_timeout(150 * MS));
        let (_, error) = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert_eq!(error.map(|e| e.code()), Some(ErrorCode::IdleTimeout));

        // Bytes every 30 ms for well over the idle limit: never cut. The limit
        // is 33x the gap so a stalled shared runner cannot fake silence; the
        // stream still runs past it (asserted below), which is the point.
        let server = MockServer::always(Reply::sse(body).pieces(8, Duration::from_millis(30)))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")).idle_timeout(1000 * MS));
        let started = Instant::now();
        let (frames, error) = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(frames, events());
        assert!(
            started.elapsed() > Duration::from_millis(1000),
            "{:?}",
            started.elapsed()
        );
    });
}

#[test]
fn total_timeout_is_enforced_while_streaming() {
    block(async {
        // 64-byte pieces every 200 ms: the first event (60 bytes) is whole
        // after the first piece, about 500 ms before the 700 ms deadline, so
        // slow shared runners and coarse timers keep plenty of slack; the
        // whole body needs well over a second, so the deadline cuts it.
        let server = MockServer::always(ok_reply().pieces(64, Duration::from_millis(200)))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")).total_timeout(700 * MS));
        let started = Instant::now();
        let (frames, error) = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert_eq!(error.map(|e| e.code()), Some(ErrorCode::Timeout));
        assert!(!frames.is_empty(), "the stream had started");
        assert!(frames.len() < events().len());
        assert!(started.elapsed() < Duration::from_secs(3));

        // A silent stream is cut at the deadline, not at the next byte.
        let server = MockServer::always(ok_reply().pieces(40, Duration::from_millis(1500)))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")).total_timeout(250 * MS));
        let started = Instant::now();
        let (_, error) = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert_eq!(error.map(|e| e.code()), Some(ErrorCode::Timeout));
        assert!(started.elapsed() < Duration::from_millis(1200));

        // The same deadline also bounds the wait for headers.
        let server = MockServer::always(ok_reply().head_delay(Duration::from_secs(5)))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")).total_timeout(150 * MS));
        let error = expect_code(transport.open(request()).await, ErrorCode::Timeout);
        assert!(!error.is_retryable(), "{error}");
    });
}

#[cfg(any(feature = "rustls", feature = "native-tls"))]
#[test]
fn connect_timeout_covers_the_tls_handshake() {
    block(async {
        // TCP connects, the handshake never completes.
        let silent = SilentServer::start()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(
            HttpTransport::builder(format!("https://{}/", silent.addr()))
                .connect_timeout(200 * MS)
                .response_timeout(Duration::from_secs(10)),
        );
        let started = Instant::now();
        let error = expect_code(transport.open(request()).await, ErrorCode::Timeout);
        assert!(started.elapsed() < Duration::from_secs(3), "{error}");
        assert!(error.message().contains("connect"), "{error}");
        // Nothing was sent: retryable, as the transport's policy treats it.
        assert!(error.is_retryable(), "{error}");
    });
}

#[test]
fn refused_connections_are_connect_errors() {
    block(async {
        // The refused address is a port bound and released at once. Servers
        // started by other tests in this binary can be handed the same port
        // in the meantime, so an attempt that reaches something other than a
        // refusal is retried on a fresh port rather than failed.
        for _ in 0..5 {
            let addr = {
                let listener =
                    std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
                listener.local_addr().unwrap_or_else(|e| panic!("{e}"))
            };
            let transport = build(HttpTransport::builder(format!("http://{addr}/")));
            let error = match transport.open(request()).await {
                Err(e) if e.code() == ErrorCode::Connect => e,
                _ => continue, // the port was reused
            };
            assert!(error.is_retryable());
            // Connect errors are retried (nothing was sent): two backoffs pass.
            let mut policy = RetryPolicy::exponential(2);
            policy.initial_backoff = 150 * MS;
            policy.jitter = false;
            let transport = build(HttpTransport::builder(format!("http://{addr}/")).retry(policy));
            let started = Instant::now();
            match transport.open(request()).await {
                Err(e) if e.code() == ErrorCode::Connect => {}
                _ => continue, // the port was reused
            }
            assert!(started.elapsed() >= 450 * MS, "{:?}", started.elapsed());
            return;
        }
        panic!("every released port was reused before the test could refuse on it");
    });
}

/// A connect-phase timeout (here the TLS handshake) is a connect failure:
/// nothing was sent, so the policy retries it as it retries a refusal. The
/// surfaced code stays `transport.timeout`.
#[cfg(any(feature = "rustls", feature = "native-tls"))]
#[test]
fn connect_timeouts_are_retried() {
    block(async {
        let silent = SilentServer::start()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let mut policy = RetryPolicy::exponential(2);
        policy.initial_backoff = 50 * MS;
        policy.jitter = false;
        let transport = build(
            HttpTransport::builder(format!("https://{}/", silent.addr()))
                .connect_timeout(150 * MS)
                .response_timeout(Duration::from_secs(10))
                .retry(policy),
        );
        let started = Instant::now();
        expect_code(transport.open(request()).await, ErrorCode::Timeout);
        // Three attempts of 150 ms plus backoffs of 50 and 100 ms.
        assert!(started.elapsed() >= 550 * MS, "{:?}", started.elapsed());
    });
}

/// When the wait before a retry would pass the total deadline, the failure
/// that prompted it is reported, not a bare total timeout.
#[test]
fn a_retry_past_the_deadline_reports_the_failure() {
    block(async {
        let server = MockServer::always(
            Reply::status(503)
                .header("Retry-After", "30")
                .framing(Framing::Length),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(
            HttpTransport::builder(server.url("/"))
                .retry(RetryPolicy::exponential(1))
                .total_timeout(Duration::from_secs(2)),
        );
        let error = expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert_eq!(error.http_status(), Some(503));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(30)));
        assert_eq!(server.requests().len(), 1);

        // As in refused_connections_are_connect_errors: a released port can
        // be handed to another test's server, so an attempt that reaches
        // something other than a refusal is retried on a fresh port.
        for _ in 0..5 {
            let addr = {
                let listener =
                    std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
                listener.local_addr().unwrap_or_else(|e| panic!("{e}"))
            };
            // Windows takes about two seconds to report a refused loopback
            // connect, so the deadline leaves room for that; the backoff is
            // past it on every platform.
            let mut policy = RetryPolicy::exponential(1);
            policy.initial_backoff = Duration::from_secs(30);
            policy.jitter = false;
            let transport = build(
                HttpTransport::builder(format!("http://{addr}/"))
                    .retry(policy)
                    .total_timeout(Duration::from_secs(4)),
            );
            match transport.open(request()).await {
                Err(e) if e.code() == ErrorCode::Connect => return,
                _ => continue, // the port was reused
            }
        }
        panic!("no attempt was refused");
    });
}

// ---------------------------------------------------------------- retry

#[test]
fn retries_before_headers_on_retryable_statuses() {
    for status in [429u16, 502, 503, 504] {
        block(async move {
            let server = MockServer::sequence(vec![
                Reply::status(status).framing(Framing::Length),
                Reply::status(status)
                    .header("Retry-After", "0")
                    .framing(Framing::Length),
                ok_reply(),
            ])
            .await
            .unwrap_or_else(|e| panic!("{e}"));
            let mut policy = RetryPolicy::exponential(3);
            policy.initial_backoff = 10 * MS;
            let transport = build(HttpTransport::builder(server.url("/")).retry(policy));
            let (frames, error) = drain(
                transport
                    .open(request())
                    .await
                    .unwrap_or_else(|e| panic!("{e}")),
            )
            .await;
            assert!(error.is_none());
            assert_eq!(frames, events());
            let requests = server.requests();
            assert_eq!(requests.len(), 3, "{status}");
            assert!(requests.iter().all(|r| r.json() == Some(input())));
        });
    }
}

#[test]
fn retry_limits_and_non_retryable_failures() {
    block(async {
        // Retries exhausted: the last status is reported.
        let server = MockServer::always(Reply::status(503).framing(Framing::Length))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let mut policy = RetryPolicy::exponential(2);
        policy.initial_backoff = MS;
        policy.jitter = false;
        let transport = build(HttpTransport::builder(server.url("/")).retry(policy));
        let error = expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert_eq!(error.http_status(), Some(503));
        assert_eq!(server.requests().len(), 3);

        // Not retryable: 500, and anything without a policy.
        let server = MockServer::always(Reply::status(500).framing(Framing::Length))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")).retry(policy));
        expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert_eq!(server.requests().len(), 1);
        let server = MockServer::always(Reply::status(503).framing(Framing::Length))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")));
        expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert_eq!(server.requests().len(), 1);
    });
}

#[test]
fn never_retries_after_the_stream_started() {
    block(async {
        let cut = sse(&events()[..2]).len() + 3;
        let server = MockServer::always(ok_reply().cut_after(cut))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport =
            build(HttpTransport::builder(server.url("/")).retry(RetryPolicy::exponential(5)));
        let (frames, error) = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert_eq!(frames.len(), 2);
        let error = error.unwrap_or_else(|| panic!("no error"));
        assert_eq!(error.code(), ErrorCode::Truncated);
        assert!(!error.is_retryable());
        sleep(100).await;
        assert_eq!(
            server.requests().len(),
            1,
            "a replay would duplicate the run"
        );
    });
}

#[test]
fn retry_after_is_honoured_up_to_its_cap() {
    block(async {
        let server = MockServer::sequence(vec![
            Reply::status(429)
                .header("Retry-After", "1")
                .framing(Framing::Length),
            ok_reply(),
        ])
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let mut policy = RetryPolicy::exponential(1);
        policy.initial_backoff = MS;
        policy.jitter = false;
        let transport = build(HttpTransport::builder(server.url("/")).retry(policy));
        let started = Instant::now();
        let _ = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert!(
            started.elapsed() >= Duration::from_millis(950),
            "waited for Retry-After"
        );
        assert_eq!(server.requests().len(), 2);

        // Longer than the cap: not retried, the delay reported.
        let server = MockServer::sequence(vec![
            Reply::status(503)
                .header("Retry-After", "120")
                .framing(Framing::Length),
            ok_reply(),
        ])
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let mut capped = policy;
        capped.max_retry_after = Duration::from_secs(5);
        let transport = build(HttpTransport::builder(server.url("/")).retry(capped));
        let error = expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(120)));
        assert_eq!(server.requests().len(), 1);
    });
}

#[test]
fn retry_policy_backoff() {
    let p = RetryPolicy::exponential(4);
    assert_eq!(p.backoff(0), 200 * MS);
    assert_eq!(p.backoff(1), 400 * MS);
    assert_eq!(p.backoff(3), 1600 * MS);
    assert_eq!(p.backoff(10), Duration::from_secs(10));
    assert_eq!(p.backoff(u32::MAX), Duration::from_secs(10));
    assert!(p.jitter && p.honor_retry_after);
    assert_eq!(RetryPolicy::default(), RetryPolicy::none());
    assert_eq!(RetryPolicy::none().max_retries, 0);
    let limits = HttpLimits::default();
    assert_eq!(limits.max_error_body_bytes, 64 * 1024);
    assert_eq!(limits.error_body_timeout, Duration::from_secs(5));
}

// ---------------------------------------------------------------- bodies and types

/// The rejection body in the error's source chain is peer-chosen text, escaped
/// like `Error`'s own `Display`, so a chain-walking logger cannot be made to
/// forge lines.
#[test]
fn error_body_display_is_escaped() {
    block(async {
        let server = MockServer::always(
            Reply::status(500).body(b"line1\n\x1b[31mFAKE\r\nINFO forged".to_vec()),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")));
        let error = expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        let body = std::error::Error::source(&error)
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert_eq!(body, "line1\\n\\u{1b}[31mFAKE\\r\\nINFO forged");
        assert!(!body.contains(['\n', '\r', '\x1b']), "{body:?}");
    });
}

#[test]
fn error_bodies_are_bounded() {
    block(async {
        let server = MockServer::always(
            Reply::status(400)
                .body(vec![b'e'; 200_000])
                .pieces(10_000, Duration::ZERO),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let mut limits = HttpLimits::default();
        limits.max_error_body_bytes = 32;
        let transport = build(HttpTransport::builder(server.url("/")).limits(limits));
        let error = expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert_eq!(error.http_status(), Some(400));
        let body = std::error::Error::source(&error)
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert_eq!(body, format!("{} [...]", "e".repeat(32)));

        // A body that never ends is abandoned at the time budget.
        let server = MockServer::always(
            Reply::status(500)
                .body(vec![b'x'; 64])
                .pieces(1, Duration::from_millis(200)),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let mut limits = HttpLimits::default();
        limits.error_body_timeout = 300 * MS;
        let transport = build(HttpTransport::builder(server.url("/")).limits(limits));
        let started = Instant::now();
        let error = expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(error.http_status(), Some(500));
    });
}

#[test]
fn the_parser_is_chosen_by_the_exact_content_type() {
    let cases: [(&str, bool); 6] = [
        ("text/event-stream", true),
        ("Text/Event-Stream; charset=utf-8", true),
        ("text/event-stream-v2", false),
        ("text/html", false),
        ("application/json", false),
        ("application/vnd.ag-ui.event+proto", cfg!(feature = "proto")),
    ];
    for (content_type, accepted) in cases {
        let result = block(async move {
            let body = if content_type.contains("proto") {
                #[cfg(feature = "proto")]
                {
                    proto(&events())
                }
                #[cfg(not(feature = "proto"))]
                {
                    Vec::new()
                }
            } else {
                sse(&events())
            };
            let server = MockServer::always(
                Reply::status(200)
                    .header("Content-Type", content_type)
                    .body(body),
            )
            .await
            .unwrap_or_else(|e| panic!("{e}"));
            let transport = build(HttpTransport::builder(server.url("/")));
            match transport.open(request()).await {
                Ok(stream) => Ok(drain(stream).await.0),
                Err(e) => Err(e),
            }
        });
        if accepted {
            assert_eq!(result.ok(), Some(events()), "{content_type}");
        } else {
            expect_code(result, ErrorCode::UnsupportedContentType);
        }
    }
    let missing = block(async {
        let server = MockServer::always(Reply::status(200).body(sse(&events())))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        build(HttpTransport::builder(server.url("/")))
            .open(request())
            .await
            .map(|_| ())
    });
    expect_code(missing, ErrorCode::UnsupportedContentType);
}

/// covers: PB-17
#[cfg(feature = "proto")]
#[test]
fn unknown_protobuf_arms_are_dropped_with_a_warning() {
    use agui_sdk::diagnostics::{CollectingSink, DiagnosticCode};
    use std::sync::Arc;
    let mut body = proto(&events()[..1]);
    // An envelope whose only field is number 999 (a newer event type).
    let unknown = [0xBA, 0x3E, 0x00];
    body.extend_from_slice(&(unknown.len() as u32).to_be_bytes());
    body.extend_from_slice(&unknown);
    body.extend_from_slice(&proto(&events()[1..]));
    let sink = Arc::new(CollectingSink::new());
    let mut req = request();
    req.diagnostics = sink.clone();
    let (frames, error) = block(async move {
        let server = MockServer::always(Reply::proto(body).pieces(3, Duration::ZERO))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        drain(
            build(HttpTransport::builder(server.url("/")))
                .open(req)
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await
    });
    assert!(error.is_none(), "{error:?}");
    assert_eq!(frames, events());
    let codes: Vec<_> = sink.take().into_iter().map(|d| d.code).collect();
    assert_eq!(codes, [DiagnosticCode::UnknownProtobufArm]);
}

/// covers: PROC-06
///
/// An `int64` outside the safe range is judged after middleware on both wires:
/// a middleware that drops it saves the run over protobuf as over SSE, and
/// with no middleware both wires fail with the same code.
#[cfg(feature = "proto")]
#[test]
fn an_oversize_int64_is_judged_after_middleware_on_both_wires() {
    use agui_sdk::client::middleware::{map_frames, MiddlewareRequest, Next};
    use agui_sdk::client::{AgentClient, Frame, RunRequest};
    use agui_sdk::diagnostics::NullSink;
    use std::future::IntoFuture;

    const BIG: u64 = 9_007_199_254_740_993; // 2^53 + 1
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    // Event{TEXT_MESSAGE_START{base_event{timestamp: BIG}, message_id: "m-r1"}}.
    let mut base = vec![0x10];
    varint(BIG, &mut base);
    let mut start = vec![0x0a, base.len() as u8];
    start.extend_from_slice(&base);
    start.extend_from_slice(&[0x12, 4]);
    start.extend_from_slice(b"m-r1");
    let mut envelope = vec![0x0a, start.len() as u8];
    envelope.extend_from_slice(&start);

    let mut json_events = events();
    json_events[1]["timestamp"] = json!(BIG);
    let mut proto_body = proto(&events()[..1]);
    agui_sdk::proto::framing::write_frame(&envelope, &mut proto_body)
        .unwrap_or_else(|e| panic!("{e}"));
    proto_body.extend_from_slice(&proto(&events()[2..]));
    let sse_body = sse(&json_events);

    let run_over = |reply: Reply, strip: bool| {
        block(async move {
            let server = MockServer::always(reply)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let mut builder =
                AgentClient::with_transport(build(HttpTransport::builder(server.url("/"))))
                    .diagnostic_sink(NullSink);
            if strip {
                builder = builder.middleware(move |request: MiddlewareRequest, next: Next| {
                    map_frames(next.run(request), move |mut frame: Frame| {
                        if let Some(o) = frame.value_mut()?.as_object_mut() {
                            o.remove("timestamp");
                        }
                        Ok(Some(frame))
                    })
                });
            }
            let client = builder.build().unwrap_or_else(|e| panic!("{e}"));
            client
                .run(RunRequest::new().thread_id("t1").run_id("r1").user("hi"))
                .into_future()
                .await
        })
    };
    for (wire, body) in [("sse", sse_body), ("proto", proto_body)] {
        let reply = || {
            if wire == "sse" {
                Reply::sse(body.clone())
            } else {
                Reply::proto(body.clone())
            }
        };
        let saved = run_over(reply(), true).unwrap_or_else(|e| panic!("{wire}: {e}"));
        assert!(saved.status.is_success(), "{wire}");
        match run_over(reply(), false) {
            Ok(_) => panic!("{wire}: expected InvalidValue, got Ok"),
            Err(e) => assert_eq!(e.code(), ErrorCode::InvalidValue, "{wire}: {e}"),
        }
    }
}

#[cfg(feature = "proto")]
#[test]
fn accept_follows_the_requested_formats() {
    use agui_sdk::wire::WireFormat;
    let captured = block(async {
        let server = MockServer::always(ok_reply())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")));
        for accept in [
            vec![WireFormat::Sse],
            vec![WireFormat::Protobuf, WireFormat::Sse],
        ] {
            let mut req = request();
            req.accept = accept;
            let _ = drain(transport.open(req).await.unwrap_or_else(|e| panic!("{e}"))).await;
        }
        server.requests()
    });
    assert_eq!(captured[0].header("accept"), Some("text/event-stream"));
    assert_eq!(
        captured[1].header("accept"),
        Some("application/vnd.ag-ui.event+proto, text/event-stream")
    );
    // A protobuf body to a client that offered only SSE is refused.
    let refused = block(async {
        let server = MockServer::always(Reply::proto(proto(&events())))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let mut req = request();
        req.accept = vec![WireFormat::Sse];
        build(HttpTransport::builder(server.url("/")))
            .open(req)
            .await
            .map(|_| ())
    });
    expect_code(refused, ErrorCode::UnsupportedContentType);
}

#[test]
fn sse_framing_errors_end_the_stream() {
    let (frames, error) = block(async {
        let mut body = sse(&events()[..1]);
        body.extend_from_slice(b"data: \xFF\xFE\n\n");
        let server = MockServer::always(Reply::sse(body))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        drain(
            build(HttpTransport::builder(server.url("/")))
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await
    });
    assert_eq!(frames.len(), 1);
    assert_eq!(error.map(|e| e.code()), Some(ErrorCode::SseFraming));
}

// ---------------------------------------------------------------- cancellation

#[test]
fn cancellation_interrupts_waiting_and_streaming() {
    block(async {
        let server = MockServer::always(ok_reply().head_delay(Duration::from_secs(5)))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")));
        let req = request();
        let cancel = req.cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            cancel.cancel();
        });
        let started = Instant::now();
        expect_code(transport.open(req).await, ErrorCode::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = canceller.join();

        let server = MockServer::always(ok_reply().pieces(10, Duration::from_millis(100)))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")));
        let req = request();
        let cancel = req.cancel.clone();
        let mut stream = transport.open(req).await.unwrap_or_else(|e| panic!("{e}"));
        let first = stream.next().await;
        assert!(matches!(first, Some(Ok(_))));
        cancel.cancel();
        let next = stream.next().await;
        assert_eq!(
            next.and_then(|r| r.err()).map(|e| e.code()),
            Some(ErrorCode::Cancelled)
        );
        assert!(stream.next().await.is_none());

        // Frames already buffered are not handed out after cancellation.
        let server = MockServer::always(ok_reply())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/")));
        let req = request();
        let cancel = req.cancel.clone();
        let mut stream = transport.open(req).await.unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(stream.next().await, Some(Ok(_))));
        cancel.cancel();
        let next = stream.next().await;
        assert_eq!(
            next.and_then(|r| r.err()).map(|e| e.code()),
            Some(ErrorCode::Cancelled)
        );
    });
}

// ---------------------------------------------------------------- proxy

#[test]
fn plain_http_through_an_authenticating_proxy() {
    block(async {
        let server = MockServer::always(ok_reply())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let proxy = MockProxy::start(Some("pu:pp"))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let url = proxy.url().replace("http://", "http://pu:pp@");
        let transport = build(HttpTransport::builder(server.url("/run")).proxy(url));
        let (frames, error) = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert!(error.is_none());
        assert_eq!(frames, events());
        let seen = proxy.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].target, server.url("/run"), "absolute-form");
        assert!(seen[0].header("proxy-authorization").is_some());

        // Wrong credentials: the proxy's 407 is a pre-stream rejection.
        let url = proxy.url().replace("http://", "http://pu:wrong@");
        let transport = build(HttpTransport::builder(server.url("/run")).proxy(url));
        let error = expect_code(transport.open(request()).await, ErrorCode::HttpStatus);
        assert_eq!(error.http_status(), Some(407));
    });
}

// ---------------------------------------------------------------- TLS

#[cfg(feature = "rustls")]
mod rustls_tls {
    use super::*;
    use agui_sdk::client::transport::{TlsBackend, TlsRoots};

    fn https(server: &MockServer) -> HttpTransportBuilder {
        HttpTransport::builder(server.url("/run")).tls_backend(TlsBackend::Rustls)
    }

    #[test]
    fn custom_roots_from_der_and_pem() {
        block(async {
            let server = MockServer::start_tls(tls::server_config(false), |_| ok_reply())
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            // The bundled roots do not know the test CA.
            let untrusted = build(https(&server));
            expect_code(untrusted.open(request()).await, ErrorCode::Connect);
            for builder in [
                https(&server).add_root_certificate_der(tls::file("ca.der")),
                https(&server).add_root_certificates_pem(tls::file("ca.pem")),
                https(&server)
                    .tls_roots(TlsRoots::CustomOnly)
                    .add_root_certificates_pem(
                        [tls::file("other-ca.pem"), tls::file("ca.pem")].concat(),
                    ),
            ] {
                let transport = build(builder);
                let (frames, error) = drain(
                    transport
                        .open(request())
                        .await
                        .unwrap_or_else(|e| panic!("{e}")),
                )
                .await;
                assert!(error.is_none());
                assert_eq!(frames, events());
            }
            let wrong = build(
                https(&server)
                    .tls_roots(TlsRoots::CustomOnly)
                    .add_root_certificates_pem(tls::file("other-ca.pem")),
            );
            expect_code(wrong.open(request()).await, ErrorCode::Connect);
        });
    }

    #[test]
    fn mutual_tls_with_a_pem_identity() {
        block(async {
            let server = MockServer::start_tls(tls::server_config(true), |_| ok_reply())
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let anonymous = build(https(&server).add_root_certificates_pem(tls::file("ca.pem")));
            let refused = match anonymous.open(request()).await {
                Err(e) => Some(e.code()),
                Ok(stream) => drain(stream).await.1.map(|e| e.code()),
            };
            assert!(
                refused.is_some(),
                "the server requires a client certificate"
            );
            let identified = build(
                https(&server)
                    .add_root_certificates_pem(tls::file("ca.pem"))
                    .client_identity_pem(tls::file("client.pem"), tls::file("client.key")),
            );
            let (frames, error) = drain(
                identified
                    .open(request())
                    .await
                    .unwrap_or_else(|e| panic!("{e}")),
            )
            .await;
            assert!(error.is_none());
            assert_eq!(frames, events());
            assert!(server.requests().iter().any(|r| r.client_cert));
        });
    }

    #[test]
    fn https_through_a_connect_tunnel() {
        block(async {
            let server = MockServer::start_tls(tls::server_config(false), |_| ok_reply())
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let proxy = MockProxy::start(Some("u:p"))
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let transport = build(
                https(&server)
                    .add_root_certificate_der(tls::file("ca.der"))
                    .proxy(proxy.url().replace("http://", "http://u:p@")),
            );
            let (frames, error) = drain(
                transport
                    .open(request())
                    .await
                    .unwrap_or_else(|e| panic!("{e}")),
            )
            .await;
            assert!(error.is_none(), "{error:?}");
            assert_eq!(frames, events());
            let seen = proxy.seen();
            assert_eq!(seen[0].method, "CONNECT");
            assert_eq!(seen[0].target, server.addr().to_string());
            assert!(seen[0].header("proxy-authorization").is_some());
            let at_server = server.last_request().unwrap_or_default();
            assert_eq!(
                at_server.header("proxy-authorization"),
                None,
                "not leaked inside the tunnel"
            );
        });
    }

    #[test]
    fn invalid_tls_material_is_refused_at_build() {
        let base = || HttpTransport::builder("https://127.0.0.1:1/");
        for builder in [
            base().add_root_certificate_der(b"not der".to_vec()),
            base().add_root_certificates_pem(b"no pem here".to_vec()),
            base().tls_roots(TlsRoots::CustomOnly),
            base().client_identity_pem(tls::file("client.pem"), b"not a key".to_vec()),
            base().client_identity_pem(b"".to_vec(), tls::file("client.key")),
        ] {
            expect_code(builder.build(), ErrorCode::InvalidConfig);
        }
        #[cfg(not(feature = "rustls-native-roots"))]
        expect_code(
            base().tls_roots(TlsRoots::Native).build(),
            ErrorCode::InvalidConfig,
        );
        #[cfg(not(feature = "native-tls"))]
        expect_code(
            base().tls_backend(TlsBackend::NativeTls).build(),
            ErrorCode::InvalidConfig,
        );
    }
}

#[cfg(feature = "native-tls")]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS distrusts server certificates valid for over 398 days; the test PKI's last a century"
)]
fn native_tls_with_a_custom_root() {
    use agui_sdk::client::transport::TlsBackend;
    block(async {
        let server = MockServer::start_tls(tls::server_config(false), |_| ok_reply())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(
            HttpTransport::builder(server.url("/"))
                .tls_backend(TlsBackend::NativeTls)
                .add_root_certificates_pem(tls::file("ca.pem")),
        );
        let (frames, error) = drain(
            transport
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(frames, events());
    });
}

/// A PEM bundle whose last certificate is cut off is refused, not trusted
/// short of that root, under either backend.
#[cfg(feature = "native-tls")]
#[test]
fn native_tls_refuses_a_truncated_pem_bundle() {
    use agui_sdk::client::transport::TlsBackend;
    let mut bundle = tls::file("ca.pem");
    bundle.extend_from_slice(b"\n-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIU\n");
    expect_code(
        HttpTransport::builder("https://127.0.0.1:1/")
            .tls_backend(TlsBackend::NativeTls)
            .add_root_certificates_pem(bundle)
            .build(),
        ErrorCode::InvalidConfig,
    );
}

// ---------------------------------------------------------------- capabilities

#[test]
fn a_capability_body_cut_short_is_a_read_error_not_a_rejected_declaration() {
    block(async {
        let server = MockServer::always(
            Reply::status(200)
                .header("Content-Type", "application/json")
                .body(r#"{"transport":{"streaming":true}}"#)
                .framing(Framing::Length)
                .cut_after(5),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(HttpTransport::builder(server.url("/run")));
        expect_code(
            transport
                .fetch_capabilities(&CapabilitiesRequest::new(server.url("/caps")))
                .await,
            ErrorCode::StreamRead,
        );
    });
}

#[test]
fn fetches_and_validates_capabilities() {
    block(async {
        let declaration = json!({"transport": {"streaming": true}, "identity": {"name": "demo"}});
        let body = declaration.to_string();
        // Valid apart from its size, so only the cap can refuse it.
        let big = json!({"identity": {"name": "x".repeat(200)}}).to_string();
        let big_len = big.len();
        let server = MockServer::start(move |req| {
            if req.target == "/caps" {
                Reply::status(200)
                    .header("Content-Type", "application/json")
                    .body(body.clone())
            } else if req.target == "/big" {
                Reply::status(200)
                    .header("Content-Type", "application/json")
                    .body(big.clone())
            } else if req.target == "/bad" {
                Reply::status(200).body(r#"{"transport": {"streaming": "yes"}}"#)
            } else {
                Reply::status(404).body("nope")
            }
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let transport = build(
            HttpTransport::builder(server.url("/run"))
                .bearer_token("transport-token")
                .header("X-Tenant", "acme"),
        );
        let ingested = transport
            .fetch_capabilities(
                &CapabilitiesRequest::new(server.url("/caps")).header("X-Tenant", "beta"),
            )
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(ingested.diagnostics.is_empty());
        let got = server.last_request().unwrap_or_default();
        assert_eq!(got.method, "GET");
        assert_eq!(got.header("accept"), Some("application/json"));
        assert_eq!(got.header("authorization"), Some("Bearer transport-token"));
        assert_eq!(
            got.header_all("x-tenant"),
            ["beta"],
            "the request's header wins"
        );

        let small = CapabilitiesRequest::new(server.url("/big")).max_bytes(100);
        let refused = expect_code(
            transport.fetch_capabilities(&small).await,
            ErrorCode::CapabilitiesRejected,
        );
        assert!(
            refused.message().contains("exceeds 100 bytes"),
            "{}",
            refused.message()
        );
        let exact = CapabilitiesRequest::new(server.url("/big")).max_bytes(big_len);
        transport
            .fetch_capabilities(&exact)
            .await
            .unwrap_or_else(|e| panic!("a body of exactly max_bytes is within the cap: {e}"));
        let invalid = CapabilitiesRequest::new(server.url("/bad"));
        expect_code(
            transport.fetch_capabilities(&invalid).await,
            ErrorCode::CapabilitiesRejected,
        );
        let missing = CapabilitiesRequest::new(server.url("/none"));
        let error = expect_code(
            transport.fetch_capabilities(&missing).await,
            ErrorCode::HttpStatus,
        );
        assert_eq!(error.http_status(), Some(404));
        expect_code(
            transport
                .fetch_capabilities(&CapabilitiesRequest::new("not a url"))
                .await,
            ErrorCode::InvalidRequest,
        );

        let slow = MockServer::always(
            Reply::status(200)
                .body("{}")
                .head_delay(Duration::from_secs(5)),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let timed = CapabilitiesRequest::new(slow.url("/")).timeout(150 * MS);
        expect_code(
            transport.fetch_capabilities(&timed).await,
            ErrorCode::Timeout,
        );
        let debug = format!(
            "{:?}",
            CapabilitiesRequest::new("http://u:pw@h/c?k=s").bearer_token("tt")
        );
        assert!(
            !debug.contains("tt\"") && !debug.contains("pw") && !debug.contains("k=s"),
            "{debug}"
        );
    });
}

// ------------------------------------------- security audit 2 regressions

/// M3: a capability declaration on another origin never receives the
/// transport's credentials (bearer token, URL userinfo, custom headers),
/// by the same rule redirects follow; the request's own headers still go.
#[test]
fn capabilities_on_another_origin_get_no_transport_credentials() {
    block(async {
        let caps = || {
            Reply::status(200)
                .header("Content-Type", "application/json")
                .body(r#"{"transport": {"streaming": true}}"#)
        };
        let agent = MockServer::always(caps())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let other = MockServer::always(caps())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let url = format!("http://user:pw@{}/agent", agent.addr());
        let transport = build(
            HttpTransport::builder(url)
                .bearer_token("SECRET-AGENT-TOKEN")
                .header("X-Api-Key", "SECRET-KEY")
                .header("X-Tenant", "acme"),
        );
        transport
            .fetch_capabilities(
                &CapabilitiesRequest::new(other.url("/caps")).header("X-Caps", "own"),
            )
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let got = other.last_request().unwrap_or_default();
        assert_eq!(got.header("authorization"), None, "{got:?}");
        assert_eq!(got.header("x-api-key"), None, "{got:?}");
        assert_eq!(got.header("x-tenant"), None, "{got:?}");
        assert_eq!(got.header("x-caps"), Some("own"));
        // The same origin still gets them.
        transport
            .fetch_capabilities(&CapabilitiesRequest::new(agent.url("/caps")))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let got = agent.last_request().unwrap_or_default();
        assert_eq!(
            got.header("authorization"),
            Some("Bearer SECRET-AGENT-TOKEN")
        );
        assert_eq!(got.header("x-api-key"), Some("SECRET-KEY"));
    });
}

/// The request's own headers override the capabilities URL's userinfo,
/// and every value of a repeated request header is sent, as on a run.
#[test]
fn capabilities_request_headers_override_and_repeat() {
    block(async {
        let server = MockServer::always(
            Reply::status(200)
                .header("Content-Type", "application/json")
                .body(r#"{"transport": {"streaming": true}}"#),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let transport =
            build(HttpTransport::builder(server.url("/agent")).header("X-Scope", "base"));
        let caps = format!("http://u:p@{}/caps", server.addr());
        transport
            .fetch_capabilities(
                &CapabilitiesRequest::new(caps)
                    .bearer_token("t")
                    .header("X-Scope", "a")
                    .header("X-Scope", "b"),
            )
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let got = server.last_request().unwrap_or_default();
        assert_eq!(got.header_all("authorization"), ["Bearer t"], "{got:?}");
        assert_eq!(got.header_all("x-scope"), ["a", "b"], "{got:?}");
        // Without an explicit Authorization, the URL's userinfo is sent.
        let caps = format!("http://u:p@{}/caps", server.addr());
        transport
            .fetch_capabilities(&CapabilitiesRequest::new(caps))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let got = server.last_request().unwrap_or_default();
        assert_eq!(got.header_all("authorization"), ["Basic dTpw"], "{got:?}");
        assert_eq!(got.header_all("x-scope"), ["base"], "{got:?}");
    });
}

/// M1: the client's `Limits::max_frame_bytes` bounds protobuf frames as it
/// bounds SSE events, in the transport, before any decode.
#[test]
fn the_client_frame_limit_bounds_both_wires() {
    use agui_sdk::client::{AgentClient, RunRequest};
    use agui_sdk::diagnostics::NullSink;
    use agui_sdk::pipeline::Limits;
    let blob = "x".repeat(200_000);
    let events = vec![
        json!({"type": "RUN_STARTED", "threadId": "t1", "runId": "r1"}),
        json!({"type": "STATE_SNAPSHOT", "snapshot": {"blob": blob}}),
        json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": "r1"}),
    ];
    let mut replies = vec![Reply::sse(sse(&events))];
    #[cfg(feature = "proto")]
    replies.push(Reply::proto(proto(&events)));
    for reply in replies {
        let code = block(async {
            let server = MockServer::always(reply)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let mut limits = Limits::default();
            limits.max_frame_bytes = 1024;
            let client = AgentClient::http(server.url("/"))
                .limits(limits)
                .diagnostic_sink(NullSink)
                .build()
                .unwrap_or_else(|e| panic!("{e}"));
            let run = client.run(RunRequest::new().thread_id("t1").run_id("r1").user("hi"));
            run.await.map(|r| r.state)
        });
        expect_code(code, ErrorCode::FrameTooLarge);
    }
    // The transport honours the request's limit directly too, on both wires.
    let mut replies = vec![Reply::sse(sse(&events))];
    #[cfg(feature = "proto")]
    replies.push(Reply::proto(proto(&events)));
    for reply in replies {
        let (frames, error) = block(async {
            let server = MockServer::always(reply)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let transport = build(HttpTransport::builder(server.url("/")));
            let mut req = request();
            req.max_frame_bytes = 1024;
            drain(transport.open(req).await.unwrap_or_else(|e| panic!("{e}"))).await
        });
        assert_eq!(frames.len(), 1, "only RUN_STARTED fits");
        assert_eq!(error.map(|e| e.code()), Some(ErrorCode::FrameTooLarge));
    }
}

/// L5: a stream is bounded by default (idle 5 min, total 30 min); both stay
/// configurable, and `Duration::MAX` turns one off.
#[test]
fn stream_timeouts_are_on_by_default() {
    let text = format!("{:?}", build(HttpTransport::builder("http://h/")));
    assert!(text.contains("idle_timeout: Some(300s)"), "{text}");
    assert!(text.contains("total_timeout: Some(1800s)"), "{text}");
    let text = format!(
        "{:?}",
        build(
            HttpTransport::builder("http://h/")
                .idle_timeout(Duration::from_secs(7))
                .total_timeout(Duration::MAX)
        )
    );
    assert!(text.contains("idle_timeout: Some(7s)"), "{text}");
    assert!(text.contains("total_timeout: None"), "{text}");
}

/// L6: `Debug` shows header values only for known-safe names; a credential
/// under any other name is redacted, whatever the heuristics would guess.
#[test]
fn debug_redacts_every_header_not_known_safe() {
    let builder = HttpTransport::builder("http://h/")
        .header("X-Auth", "SECRET1")
        .header("Token", "SECRET2")
        .header("X-Session", "SECRET3")
        .header("Access_Token", "SECRET4")
        .header("User-Agent", "agent/1.0");
    let transport = build(
        HttpTransport::builder("http://h/")
            .header("X-Auth", "SECRET1")
            .header("Token", "SECRET2")
            .header("X-Session", "SECRET3")
            .header("Access_Token", "SECRET4")
            .header("User-Agent", "agent/1.0"),
    );
    for text in [format!("{builder:?}"), format!("{transport:?}")] {
        for secret in ["SECRET1", "SECRET2", "SECRET3", "SECRET4"] {
            assert!(!text.contains(secret), "Debug leaks {secret}: {text}");
        }
        assert!(text.contains("agent/1.0"), "{text}");
        assert!(text.contains("X-Session"), "names stay visible: {text}");
    }
}
