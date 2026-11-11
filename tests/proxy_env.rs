//! The proxy environment variables (`proxy_from_env`) and the OS trust-store
//! variables (`SSL_CERT_FILE`, `SSL_CERT_DIR`). These tests mutate the
//! process environment, so they live in their own integration-test binary: no
//! other test thread runs `getenv` while a variable changes. All tests here
//! serialise on one lock, and `EnvGuard` restores every variable it clears.

mod transport_support;

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

use agui_sdk::client::transport::{HttpTransport, HttpTransportBuilder, Transport};
use agui_sdk::ErrorCode;
use transport_support::*;

/// Every variable a test here changes.
const VARS: &[&str] = &[
    "http_proxy",
    "HTTP_PROXY",
    "https_proxy",
    "HTTPS_PROXY",
    "no_proxy",
    "NO_PROXY",
    "REQUEST_METHOD",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

static ENV: Mutex<()> = Mutex::new(());

/// Holds the lock, clears the proxy variables, and puts each back (or
/// removes it, if it was absent) on drop, even after a failed assertion.
struct EnvGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn lock() -> Self {
        let lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let saved = VARS.iter().map(|v| (*v, std::env::var_os(v))).collect();
        for v in VARS {
            std::env::remove_var(v);
        }
        EnvGuard { saved, _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn build(builder: HttpTransportBuilder) -> HttpTransport {
    builder.build().unwrap_or_else(|e| panic!("{e}"))
}

fn ok_reply() -> Reply {
    Reply::sse(sse(&events()))
}

/// The guard puts back what was there, and removes what was not.
#[test]
fn the_guard_restores_the_environment() {
    {
        let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NO_PROXY", "keep.example");
        std::env::remove_var("HTTPS_PROXY");
    }
    {
        let guard = EnvGuard::lock();
        assert!(std::env::var_os("NO_PROXY").is_none(), "cleared on entry");
        std::env::set_var("HTTPS_PROXY", "http://leak.example:1");
        drop(guard);
    }
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        std::env::var_os("NO_PROXY"),
        Some(OsString::from("keep.example"))
    );
    assert!(std::env::var_os("HTTPS_PROXY").is_none(), "removed again");
    std::env::remove_var("NO_PROXY");
}

#[test]
fn the_environment_is_read_only_when_asked() {
    let _env = EnvGuard::lock();
    block(async {
        let server = MockServer::always(ok_reply())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let proxy = MockProxy::start(None)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        std::env::set_var("http_proxy", proxy.url());
        let ignored = build(HttpTransport::builder(server.url("/")));
        let _ = drain(
            ignored
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert!(proxy.seen().is_empty(), "off by default");

        let from_env = build(HttpTransport::builder(server.url("/")).proxy_from_env(true));
        let _ = drain(
            from_env
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert_eq!(proxy.seen().len(), 1);

        std::env::set_var("no_proxy", "example.com, 127.0.0.1");
        let exempt = build(HttpTransport::builder(server.url("/")).proxy_from_env(true));
        let _ = drain(
            exempt
                .open(request())
                .await
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .await;
        assert_eq!(proxy.seen().len(), 1, "NO_PROXY exempts the host");

        let explicit = MockProxy::start(None)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let wins = build(
            HttpTransport::builder(server.url("/"))
                .proxy_from_env(true)
                .proxy(explicit.url()),
        );
        let _ = drain(wins.open(request()).await.unwrap_or_else(|e| panic!("{e}"))).await;
        assert_eq!(explicit.seen().len(), 1, "an explicit proxy wins");
        std::env::remove_var("http_proxy");
        std::env::remove_var("no_proxy");

        // A proxy variable that is not Unicode is a configuration error, not
        // a silent direct connection.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            std::env::remove_var("https_proxy");
            std::env::set_var(
                "HTTPS_PROXY",
                std::ffi::OsString::from_vec(b"http://\xffproxy:3128".to_vec()),
            );
            let built = HttpTransport::builder(server.url("/"))
                .proxy_from_env(true)
                .build();
            std::env::remove_var("HTTPS_PROXY");
            let Err(error) = built else {
                panic!("a non-Unicode proxy variable must fail the build");
            };
            assert_eq!(error.code(), ErrorCode::InvalidConfig);
            assert!(error.message().contains("HTTPS_PROXY"), "{error}");

            // The same holds for the exclusion list: ignoring it would send
            // excluded hosts through the proxy.
            std::env::set_var(
                "NO_PROXY",
                std::ffi::OsString::from_vec(b"example.com,\xff".to_vec()),
            );
            let built = HttpTransport::builder(server.url("/"))
                .proxy_from_env(true)
                .build();
            std::env::remove_var("NO_PROXY");
            let Err(error) = built else {
                panic!("a non-Unicode NO_PROXY must fail the build");
            };
            assert_eq!(error.code(), ErrorCode::InvalidConfig);
            assert!(error.message().contains("NO_PROXY"), "{error}");
        }
    });
}

/// An OS store that yields nothing, with a private root added, still builds
/// (the private PKI works) but says so in `Debug`.
#[cfg(feature = "rustls-native-roots")]
#[test]
fn an_empty_os_store_is_visible_in_debug() {
    use agui_sdk::client::transport::{TlsBackend, TlsRoots};

    let _env = EnvGuard::lock();
    let empty = std::env::temp_dir().join(format!("agui-empty-roots-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap_or_else(|e| panic!("{e}"));
    std::env::set_var("SSL_CERT_FILE", empty.join("missing.pem"));
    std::env::set_var("SSL_CERT_DIR", &empty);
    let built = HttpTransport::builder("https://127.0.0.1:1/")
        .tls_backend(TlsBackend::Rustls)
        .tls_roots(TlsRoots::Native)
        .add_root_certificates_pem(tls::file("ca.pem"))
        .build();
    let _ = std::fs::remove_dir_all(&empty);
    let transport = built.unwrap_or_else(|e| panic!("{e}"));
    let text = format!("{transport:?}");
    assert!(text.contains("loaded:"), "{text}");
    // rustls-native-certs honours these variables on Unix other than macOS,
    // where the platform store is always read.
    #[cfg(all(unix, not(target_os = "macos")))]
    assert!(text.contains("loaded: 0"), "{text}");
}
