use std::sync::Arc;
use std::time::Duration;

use ::http::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};
use ::http::Uri;
use http_body_util::Full;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use crate::capability::Ingested;
use crate::client::capabilities::CapabilitiesRequest;
use crate::client::middleware::FrameStream;
use crate::error::{Error, ErrorCode, Result};

use super::{Header, Transport, TransportRequest};

mod connect;
mod endpoint;
mod exchange;
mod tls;

use connect::{Connector, ProxyConfig, ProxyServer, ProxyTcp, Tls};
pub(crate) use endpoint::redact_url;
use endpoint::Endpoint;

/// Default connect timeout (TCP, proxy tunnel and TLS handshake).
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default wait for response headers after the request is sent.
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an idle pooled connection is kept.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Default longest silence on an accepted stream: 5 minutes.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Default longest exchange, stream included: 30 minutes.
const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A configured stream timeout: `Duration::MAX` (anything from 30 years on)
/// turns it off.
fn stream_timeout(set: Option<Duration>, default: Duration) -> Option<Duration> {
    Some(set.unwrap_or(default)).filter(|d| *d < exchange::NEVER)
}

use super::RESERVED_HEADERS;

/// Header names whose values are known not to carry credentials: the only
/// ones `Debug` shows. Any other value is redacted, whatever its name, so a
/// credential under an unforeseen name (`Token`, `X-Auth`, `X-Session`)
/// never reaches a log.
const SAFE_NAMES: &[&str] = &[
    "accept-encoding",
    "accept-language",
    "cache-control",
    "content-language",
    "dnt",
    "if-match",
    "if-none-match",
    "origin",
    "pragma",
    "request-id",
    "traceparent",
    "tracestate",
    "user-agent",
    "x-correlation-id",
    "x-request-id",
    "x-requested-with",
    "x-trace-id",
];

/// Whether a header name is on the known-safe list, so its value may be
/// shown in `Debug`. Everything else is redacted (an allow-list, not a
/// guess at which names carry credentials).
pub(crate) fn is_safe_name(name: &str) -> bool {
    let name = name.trim();
    SAFE_NAMES
        .iter()
        .any(|safe| name.eq_ignore_ascii_case(safe))
}

/// Validates one header: a legal name and value, not reserved. Values are
/// marked sensitive (kept out of hyper's debug output) when redacted.
pub(crate) fn header_pair(header: &Header, code: ErrorCode) -> Result<(HeaderName, HeaderValue)> {
    let name = HeaderName::from_bytes(header.name.trim().as_bytes())
        .map_err(|_| Error::new(code, format!("header name {:?} is not valid", header.name)))?;
    if RESERVED_HEADERS.contains(&name.as_str()) {
        return Err(Error::new(
            code,
            format!(
                "header {:?} is reserved: the transport sets it (SSE-01, SSE-02)",
                header.name
            ),
        ));
    }
    let mut value = HeaderValue::from_str(&header.value).map_err(|_| {
        Error::new(
            code,
            format!("the value of header {:?} is not valid", header.name),
        )
    })?;
    if header.redacted() {
        value.set_sensitive(true);
    }
    Ok((name, value))
}

/// HTTP transport for both bindings, over hyper 1 (HTTP/1.1) and tokio:
/// POSTs the input as JSON with an explicit `Accept` (SSE, plus protobuf with
/// feature `proto`; SSE-01, SSE-02, PB-01, PB-02), selects the parser by the
/// exact response `Content-Type` (PB-07), and treats a non-2xx status as a
/// pre-stream rejection carrying the status and a bounded error body (SSE-12,
/// PB-19). A `200` alone is never success (SSE-14). No `Last-Event-ID`, no
/// resumption (SSE-16).
///
/// Needs a tokio runtime (hyper-util spawns connection tasks). Cheap to clone:
/// clones share one connection pool.
///
/// `Debug` redacts credentials: every header value except those of a short
/// list of known-safe names (`User-Agent`, `Accept-Language`, `Cache-Control`,
/// request-id and trace-context headers), and always those added with
/// [`HttpTransportBuilder::sensitive_header`]; proxy credentials; and URL
/// userinfo, query and fragment.
#[derive(Clone)]
pub struct HttpTransport {
    inner: std::sync::Arc<HttpInner>,
}

/// The hyper-util legacy client (pooled connector, TLS per feature), the
/// endpoint and the resolved settings. Private: no hyper or rustls type is
/// part of the API.
struct HttpInner {
    client: Client<Connector, Full<bytes::Bytes>>,
    endpoint: Endpoint,
    /// The transport's own headers, validated once.
    headers: HeaderMap,
    proxy: Arc<ProxyConfig>,
    connect_timeout: Duration,
    response_timeout: Duration,
    idle_timeout: Option<Duration>,
    total_timeout: Option<Duration>,
    retry: RetryPolicy,
    limits: HttpLimits,
    max_redirects: u32,
    /// What `Debug` shows; built once, secrets already redacted.
    summary: Summary,
}

/// The `Debug` view of a transport.
struct Summary {
    url: String,
    headers: Vec<Header>,
    proxy: Option<String>,
    tls: Option<TlsBackend>,
    roots: TlsRoots,
    /// What the OS store contributed (`TlsRoots::Native` with rustls only).
    native_roots: Option<tls::NativeRoots>,
    client_identity: bool,
}

impl HttpInner {
    /// `Proxy-Authorization` for a plain-HTTP target sent through an
    /// authenticated proxy (HTTPS targets authenticate on `CONNECT`).
    fn proxy_auth_for(&self, uri: &Uri) -> Option<HeaderValue> {
        if endpoint::is_https(uri) {
            return None;
        }
        self.proxy.for_uri(uri).and_then(|p| p.auth.clone())
    }
}

impl HttpTransport {
    /// Starts configuring a transport for `url`.
    pub fn builder(url: impl Into<String>) -> HttpTransportBuilder {
        HttpTransportBuilder {
            url: url.into(),
            ..HttpTransportBuilder::default()
        }
    }

    /// GETs a capability declaration over this transport's pool, TLS, proxy
    /// and root-CA settings, and validates it (CAP-15, CAP-16). The
    /// transport's own headers (credentials included) are sent only when the
    /// capabilities URL has the agent URL's origin (scheme, host and port),
    /// the rule redirects follow; `request`'s headers are always sent and
    /// override them.
    ///
    /// # Errors
    ///
    /// Transport errors, or [`ErrorCode::CapabilitiesRejected`](crate::ErrorCode::CapabilitiesRejected).
    pub async fn fetch_capabilities(&self, request: &CapabilitiesRequest) -> Result<Ingested> {
        let body = exchange::fetch(
            &self.inner,
            request.url(),
            request.headers(),
            request.timeout_value(),
            request.max_bytes_value(),
        )
        .await?;
        crate::capability::ingest_slice(&body)
    }
}

impl Transport for HttpTransport {
    async fn open(&self, request: TransportRequest) -> Result<FrameStream> {
        exchange::open(&self.inner, request).await
    }
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = &*self.inner;
        let s = &inner.summary;
        f.debug_struct("HttpTransport")
            .field("url", &s.url)
            .field("headers", &s.headers)
            .field("proxy", &s.proxy)
            .field("tls", &s.tls)
            .field("roots", &s.roots)
            .field("native_roots", &s.native_roots)
            .field("client_identity", &s.client_identity)
            .field("connect_timeout", &inner.connect_timeout)
            .field("response_timeout", &inner.response_timeout)
            .field("idle_timeout", &inner.idle_timeout)
            .field("total_timeout", &inner.total_timeout)
            .field("retry", &inner.retry)
            .field("limits", &inner.limits)
            .field("same_origin_redirects", &inner.max_redirects)
            .finish()
    }
}

/// Which certificate authorities HTTPS trusts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TlsRoots {
    /// The Mozilla bundle compiled in through `webpki-roots` (rustls), or the
    /// platform store (native-tls). The default.
    #[default]
    Bundled,
    /// The operating system's store instead of the bundle (plus any added
    /// roots; rustls needs feature `rustls-native-roots`, `build` fails
    /// without it). If the OS store yields no certificate and roots were
    /// added, only the added roots are trusted; `Debug` shows what the store
    /// contributed.
    Native,
    /// Only the certificates added with
    /// [`HttpTransportBuilder::add_root_certificate_der`] /
    /// [`add_root_certificates_pem`](HttpTransportBuilder::add_root_certificates_pem)
    /// (private PKI).
    CustomOnly,
}

/// Which TLS implementation HTTPS uses, when both are compiled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TlsBackend {
    /// rustls (feature `rustls`). The default whenever it is compiled in.
    Rustls,
    /// The platform stack (feature `native-tls`). The default only when
    /// `rustls` is off.
    NativeTls,
}

/// Configures an [`HttpTransport`]. Infallible setters; validation in `build`.
///
/// Every TLS and proxy option takes std types or raw PEM/DER bytes, so no
/// TLS library type appears in this API.
///
/// ```no_run
/// use std::time::Duration;
///
/// use agui_sdk::client::transport::HttpTransport;
/// use agui_sdk::client::AgentClient;
///
/// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// // A private CA, mutual TLS and an egress proxy (HTTPS needs `rustls` or
/// // `native-tls`; PEM is read by you and handed over as bytes).
/// let transport = HttpTransport::builder("https://agent.internal.example/run")
///     .add_root_certificates_pem(std::fs::read("ca.pem")?)
///     .client_identity_pem(std::fs::read("client.pem")?, std::fs::read("client.key")?)
///     .proxy("http://proxy.internal.example:3128")
///     .connect_timeout(Duration::from_secs(5))
///     .idle_timeout(Duration::from_secs(60))
///     .bearer_token("secret")
///     .build()?;
/// let client = AgentClient::with_transport(transport).build()?;
/// # drop(client);
/// # Ok(())
/// # }
/// ```
///
/// [`AgentClient::http`](crate::client::AgentClient::http) takes the same
/// options on one builder when no separate transport value is needed.
#[derive(Default)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct HttpTransportBuilder {
    url: String,
    headers: Vec<Header>,
    connect_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
    total_timeout: Option<Duration>,
    retry: RetryPolicy,
    limits: HttpLimits,
    same_origin_redirects: u32,
    proxy: Option<String>,
    proxy_from_env: bool,
    roots: TlsRoots,
    root_certs_der: Vec<Vec<u8>>,
    root_certs_pem: Vec<Vec<u8>>,
    identity_pem: Option<(Vec<u8>, Vec<u8>)>,
    tls_backend: Option<TlsBackend>,
    response_timeout: Option<Duration>,
}

impl HttpTransportBuilder {
    /// Adds a header. Reserved: `Accept`, `Content-Type`, `Host`,
    /// `Content-Length`, `Transfer-Encoding`, `Connection` (`build` fails).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(Header::new(name, value));
        self
    }

    /// Adds a header redacted from `Debug`.
    pub fn sensitive_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(Header::sensitive(name, value));
        self
    }

    /// `Authorization: Bearer <token>`, redacted.
    pub fn bearer_token(self, token: impl AsRef<str>) -> Self {
        let value = format!("Bearer {}", token.as_ref());
        self.sensitive_header("Authorization", value)
    }

    /// Connect timeout (TCP plus TLS handshake). Default 30 s.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Maximum wait for the response headers once the request is sent,
    /// retries excluded. Default 60 s. The stream itself is bounded by the
    /// idle and total timeouts instead.
    pub fn response_timeout(mut self, timeout: Duration) -> Self {
        self.response_timeout = Some(timeout);
        self
    }

    /// Maximum silence between received bytes. Default 5 minutes;
    /// `Duration::MAX` turns it off.
    /// Distinct from the total timeout, so long healthy streams are not cut.
    /// A peer's keep-alive comments count as bytes, so only the total
    /// timeout bounds a stream that never ends.
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = Some(timeout);
        self
    }

    /// Maximum duration of the whole exchange, stream included. Default
    /// 30 minutes, so a peer cannot hold a run
    /// open forever; raise it for longer runs, or pass `Duration::MAX` to
    /// turn it off.
    pub fn total_timeout(mut self, timeout: Duration) -> Self {
        self.total_timeout = Some(timeout);
        self
    }

    /// Retry policy for failures before response headers. Default none.
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    /// Body and error-body budgets.
    pub fn limits(mut self, limits: HttpLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Follow up to `max` same-origin redirects (default 0). Cross-origin
    /// redirects are never followed: they would leak credentials. A followed
    /// redirect re-sends the request unchanged; it is same-origin, so its
    /// credentials were already that origin's.
    pub fn same_origin_redirects(mut self, max: u32) -> Self {
        self.same_origin_redirects = max;
        self
    }

    /// Sends every request through the HTTP proxy at `url`
    /// (`http://[user:pass@]host:port`; HTTPS targets are tunnelled with
    /// `CONNECT`). Proxy credentials are redacted from `Debug`.
    pub fn proxy(mut self, url: impl Into<String>) -> Self {
        self.proxy = Some(url.into());
        self
    }

    /// Reads `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` (and lowercase forms) at
    /// `build`. Default off: the environment is never consulted implicitly.
    /// An explicit [`proxy`](Self::proxy) wins.
    pub fn proxy_from_env(mut self, enabled: bool) -> Self {
        self.proxy_from_env = enabled;
        self
    }

    /// Which root store HTTPS trusts. Default [`TlsRoots::Bundled`].
    pub fn tls_roots(mut self, roots: TlsRoots) -> Self {
        self.roots = roots;
        self
    }

    /// Trusts one more CA certificate, DER-encoded (private PKI).
    pub fn add_root_certificate_der(mut self, der: impl Into<Vec<u8>>) -> Self {
        self.root_certs_der.push(der.into());
        self
    }

    /// Trusts every CA certificate in a PEM bundle.
    pub fn add_root_certificates_pem(mut self, pem: impl Into<Vec<u8>>) -> Self {
        self.root_certs_pem.push(pem.into());
        self
    }

    /// Presents a client certificate (mutual TLS): a PEM certificate chain,
    /// leaf first, and its PEM private key (PKCS #8, PKCS #1 or SEC1).
    /// The key is redacted from `Debug`.
    pub fn client_identity_pem(
        mut self,
        cert_chain: impl Into<Vec<u8>>,
        private_key: impl Into<Vec<u8>>,
    ) -> Self {
        self.identity_pem = Some((cert_chain.into(), private_key.into()));
        self
    }

    /// Selects the TLS implementation when both `rustls` and `native-tls`
    /// are compiled in. Default: rustls if present. `build` fails if the
    /// chosen backend is not compiled in.
    pub fn tls_backend(mut self, backend: TlsBackend) -> Self {
        self.tls_backend = Some(backend);
        self
    }

    /// Finishes.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidConfig`](crate::ErrorCode::InvalidConfig): invalid
    /// URL or proxy URL, reserved header, unparseable certificate or key, an
    /// unavailable TLS backend or root store, or an `https://` URL with no
    /// TLS feature enabled.
    pub fn build(self) -> Result<HttpTransport> {
        let endpoint = Endpoint::parse(&self.url, ErrorCode::InvalidConfig, "the agent URL")?;
        let backend = tls::select_backend(self.tls_backend)?;
        let settings = tls::TlsSettings {
            roots: self.roots,
            der: &self.root_certs_der,
            pem: &self.root_certs_pem,
            identity: self.identity_pem.as_ref(),
        };
        if backend.is_none() {
            if endpoint::is_https(&endpoint.uri) {
                return Err(Error::new(
                    ErrorCode::InvalidConfig,
                    "an https:// URL needs the `rustls` or `native-tls` feature",
                ));
            }
            if settings.customised() {
                return Err(Error::new(
                    ErrorCode::InvalidConfig,
                    "TLS options need the `rustls` or `native-tls` feature",
                ));
            }
        }

        let mut headers = HeaderMap::new();
        for header in &self.headers {
            let (name, value) = header_pair(header, ErrorCode::InvalidConfig)?;
            headers.append(name, value);
        }
        if let Some((user, pass)) = &endpoint.userinfo {
            if !headers.contains_key(AUTHORIZATION) {
                let value = endpoint::basic_auth(user, pass).ok_or_else(|| {
                    Error::new(
                        ErrorCode::InvalidConfig,
                        "URL credentials are not a valid header",
                    )
                })?;
                headers.insert(AUTHORIZATION, value);
            }
        }

        let proxy = match (&self.proxy, self.proxy_from_env) {
            (Some(url), _) => ProxyConfig::explicit(ProxyServer::parse(url)?),
            (None, true) => ProxyConfig::from_env()?,
            (None, false) => ProxyConfig::default(),
        };
        let proxy = Arc::new(proxy);

        let connect_timeout = self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        let mut tcp = HttpConnector::new();
        tcp.enforce_http(false);
        tcp.set_nodelay(true);
        tcp.set_connect_timeout(Some(connect_timeout));
        let tcp = ProxyTcp {
            tcp,
            proxy: proxy.clone(),
        };
        #[cfg_attr(not(feature = "rustls"), allow(unused_mut))]
        let mut native_roots = None;
        let tls = match backend {
            None => Tls::None,
            #[cfg(feature = "rustls")]
            Some(TlsBackend::Rustls) => {
                let (config, native) = tls::rustls_config(&settings)?;
                native_roots = native;
                Tls::Rustls(
                    hyper_rustls::HttpsConnectorBuilder::new()
                        .with_tls_config(config)
                        .https_or_http()
                        .enable_http1()
                        .wrap_connector(tcp.clone()),
                )
            }
            #[cfg(feature = "native-tls")]
            Some(TlsBackend::NativeTls) => Tls::Native(hyper_tls::HttpsConnector::from((
                tcp.clone(),
                tokio_native_tls::TlsConnector::from(tls::native_connector(&settings)?),
            ))),
            #[allow(unreachable_patterns)]
            Some(_) => {
                return Err(Error::new(
                    ErrorCode::InvalidConfig,
                    "the selected TLS backend is not compiled in",
                ))
            }
        };
        let connector = Connector {
            tcp,
            tls,
            timeout: connect_timeout,
        };
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_timer(TokioTimer::new())
            .build(connector);

        let summary = Summary {
            url: endpoint::redact_url(&self.url),
            headers: self.headers.clone(),
            proxy: proxy_display(&proxy, self.proxy_from_env && self.proxy.is_none()),
            tls: backend,
            roots: self.roots,
            native_roots,
            client_identity: self.identity_pem.is_some(),
        };
        Ok(HttpTransport {
            inner: Arc::new(HttpInner {
                client,
                endpoint,
                headers,
                proxy,
                connect_timeout,
                response_timeout: self.response_timeout.unwrap_or(DEFAULT_RESPONSE_TIMEOUT),
                idle_timeout: stream_timeout(self.idle_timeout, DEFAULT_IDLE_TIMEOUT),
                total_timeout: stream_timeout(self.total_timeout, DEFAULT_TOTAL_TIMEOUT),
                retry: self.retry,
                limits: self.limits,
                max_redirects: self.same_origin_redirects,
                summary,
            }),
        })
    }
}

/// The proxy line of `Debug`, credentials redacted.
fn proxy_display(proxy: &ProxyConfig, from_env: bool) -> Option<String> {
    if proxy.is_empty() {
        return from_env.then(|| "from environment (none set)".to_owned());
    }
    let http = proxy.http.as_ref().map(|p| p.display.as_str());
    let https = proxy.https.as_ref().map(|p| p.display.as_str());
    Some(match (http, https) {
        (Some(a), Some(b)) if a == b => a.to_owned(),
        (a, b) => format!(
            "http: {}, https: {}",
            a.unwrap_or("none"),
            b.unwrap_or("none")
        ),
    })
}

impl std::fmt::Debug for HttpTransportBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransportBuilder")
            .field("url", &endpoint::redact_url(&self.url))
            .field("headers", &self.headers)
            .field("connect_timeout", &self.connect_timeout)
            .field("response_timeout", &self.response_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("total_timeout", &self.total_timeout)
            .field("retry", &self.retry)
            .field("limits", &self.limits)
            .field("same_origin_redirects", &self.same_origin_redirects)
            .field("proxy", &self.proxy.as_deref().map(endpoint::redact_url))
            .field("proxy_from_env", &self.proxy_from_env)
            .field("roots", &self.roots)
            .field("root_certs_der", &self.root_certs_der.len())
            .field("root_certs_pem", &self.root_certs_pem.len())
            .field(
                "client_identity",
                &self.identity_pem.as_ref().map(|_| "<redacted>"),
            )
            .field("tls_backend", &self.tls_backend)
            .finish()
    }
}

/// HTTP-specific budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpLimits {
    /// Largest error body read for a pre-stream rejection. Default 64 KiB.
    /// The body is the `transport.http_status` error's
    /// [`source`](std::error::Error::source), displayed with control
    /// characters escaped (so pretty-printed JSON reads with `\n` escapes).
    pub max_error_body_bytes: usize,
    /// Time allowed to read that body. Default 5 s.
    pub error_body_timeout: Duration,
}

impl Default for HttpLimits {
    fn default() -> Self {
        HttpLimits {
            max_error_body_bytes: 64 * 1024,
            error_body_timeout: Duration::from_secs(5),
        }
    }
}

/// Retries before response headers only: connect errors and 429/502/503/504,
/// exponential backoff with optional jitter, `Retry-After` honoured up to a
/// cap (a longer `Retry-After` is not waited for: the status is reported,
/// with the delay, for the caller to decide). Never after a stream started: a replay would duplicate a server run.
/// A 502/504 from a gateway does not prove the agent never received the
/// request, so retrying on one can still duplicate a non-idempotent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetryPolicy {
    /// Attempts after the first.
    pub max_retries: u32,
    /// First backoff.
    pub initial_backoff: Duration,
    /// Backoff ceiling.
    pub max_backoff: Duration,
    /// Randomise each backoff in `[0, backoff]`.
    pub jitter: bool,
    /// Honour `Retry-After` up to `max_retry_after`; a longer one is reported
    /// with the status, not retried.
    pub honor_retry_after: bool,
    /// Longest `Retry-After` waited for; a longer one is reported, not retried.
    pub max_retry_after: Duration,
}

const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(10);
const DEFAULT_MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

impl RetryPolicy {
    /// No retries. The default.
    #[must_use]
    pub const fn none() -> Self {
        RetryPolicy {
            max_retries: 0,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
            jitter: false,
            honor_retry_after: true,
            max_retry_after: DEFAULT_MAX_RETRY_AFTER,
        }
    }

    /// `max_retries` attempts with exponential backoff from 200 ms to 10 s, jittered.
    #[must_use]
    pub const fn exponential(max_retries: u32) -> Self {
        RetryPolicy {
            max_retries,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
            jitter: true,
            honor_retry_after: true,
            max_retry_after: DEFAULT_MAX_RETRY_AFTER,
        }
    }

    /// The backoff before retry number `attempt` (0-based), before jitter.
    #[must_use]
    pub fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
        self.initial_backoff
            .checked_mul(factor)
            .unwrap_or(self.max_backoff)
            .min(self.max_backoff)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy::none()
    }
}
