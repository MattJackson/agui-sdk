//! A hand-rolled HTTP/1.1 mock server and forward proxy for transport tests.
//!
//! * [`MockServer`] binds `127.0.0.1:0` on the caller's tokio runtime,
//!   answers each request with a scripted [`Reply`] (status, headers, a body
//!   written in delayed pieces, a delayed head, a connection cut mid-body),
//!   optionally over TLS with or without client certificates, and records
//!   every request ([`Captured`]).
//! * [`MockProxy`] is a forward proxy: `CONNECT` tunnels and absolute-form
//!   requests, with optional `Proxy-Authorization` checking. It records each
//!   request head it sees.
//! * [`tls`] loads the test PKI under `tests/data/tls/`.
//!
//! One request per connection; every response carries `Connection: close`.
//! It is a test double: no pipelining, no `Expect: 100-continue`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The runtime the mock runs on, for tests that need its timers.
pub use tokio;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Largest request head read.
const MAX_HEAD: usize = 64 * 1024;

/// One request as the server saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Captured {
    /// `POST`, `GET`, `CONNECT`, ...
    pub method: String,
    /// The request target as sent (origin-form, absolute-form or authority).
    pub target: String,
    /// Headers in arrival order, names as sent.
    pub headers: Vec<(String, String)>,
    /// The body, de-chunked.
    pub body: Vec<u8>,
    /// Whether the client presented a TLS certificate.
    pub client_cert: bool,
}

impl Captured {
    /// The first header named `name`, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Every value of header `name`.
    pub fn header_all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The body as JSON.
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// How the response body is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Framing {
    /// `Transfer-Encoding: chunked`, one chunk per piece.
    Chunked,
    /// `Content-Length`.
    Length,
    /// Neither: the body ends when the connection closes.
    Close,
}

/// A scripted response.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Reply {
    /// Status code.
    pub status: u16,
    /// Headers besides framing and `Connection`.
    pub headers: Vec<(String, String)>,
    /// The whole body.
    pub body: Vec<u8>,
    /// Write the body in pieces of this size (0: one piece).
    pub piece: usize,
    /// Pause before each piece.
    pub delay: Duration,
    /// Pause before the response head.
    pub head_delay: Duration,
    /// Close the connection after this many body bytes, framing unfinished.
    pub cut_after: Option<usize>,
    /// Body framing.
    pub framing: Framing,
}

impl Reply {
    /// `status`, no headers, empty body, chunked.
    pub fn status(status: u16) -> Self {
        Reply {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            piece: 0,
            delay: Duration::ZERO,
            head_delay: Duration::ZERO,
            cut_after: None,
            framing: Framing::Chunked,
        }
    }

    /// `200 text/event-stream` with `body`.
    pub fn sse(body: impl Into<Vec<u8>>) -> Self {
        Self::status(200)
            .header("Content-Type", "text/event-stream")
            .body(body)
    }

    /// `200` of the protobuf media type with `body`.
    pub fn proto(body: impl Into<Vec<u8>>) -> Self {
        Self::status(200)
            .header("Content-Type", "application/vnd.ag-ui.event+proto")
            .body(body)
    }

    /// Adds a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Sets the body.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Writes the body in `size`-byte pieces, each after `delay`.
    pub fn pieces(mut self, size: usize, delay: Duration) -> Self {
        self.piece = size;
        self.delay = delay;
        self
    }

    /// Waits `delay` before sending the head.
    pub fn head_delay(mut self, delay: Duration) -> Self {
        self.head_delay = delay;
        self
    }

    /// Drops the connection after `bytes` body bytes.
    pub fn cut_after(mut self, bytes: usize) -> Self {
        self.cut_after = Some(bytes);
        self
    }

    /// Sets the framing.
    pub fn framing(mut self, framing: Framing) -> Self {
        self.framing = framing;
        self
    }
}

type Handler = Arc<dyn Fn(&Captured) -> Reply + Send + Sync>;

/// The running server; stops accepting when dropped.
pub struct MockServer {
    addr: SocketAddr,
    tls: bool,
    requests: Arc<Mutex<Vec<Captured>>>,
    accept: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for MockServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockServer")
            .field("addr", &self.addr)
            .field("tls", &self.tls)
            .finish()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl MockServer {
    /// A plain-HTTP server answering with `handler`. Needs a tokio runtime.
    ///
    /// # Errors
    ///
    /// Binding failed.
    pub async fn start(
        handler: impl Fn(&Captured) -> Reply + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        Self::launch(Arc::new(handler), None).await
    }

    /// An HTTPS server with the test PKI's server certificate
    /// ([`tls::server_config`]).
    ///
    /// # Errors
    ///
    /// Binding failed.
    pub async fn start_tls(
        config: Arc<rustls::ServerConfig>,
        handler: impl Fn(&Captured) -> Reply + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        Self::launch(Arc::new(handler), Some(config)).await
    }

    /// Answers every request with `reply`.
    ///
    /// # Errors
    ///
    /// Binding failed.
    pub async fn always(reply: Reply) -> std::io::Result<Self> {
        Self::start(move |_| reply.clone()).await
    }

    /// Answers request *n* with `replies[n]`, later ones with the last.
    ///
    /// # Errors
    ///
    /// Binding failed.
    pub async fn sequence(replies: Vec<Reply>) -> std::io::Result<Self> {
        let next = Mutex::new(0usize);
        Self::start(move |_| {
            let mut n = next.lock().unwrap_or_else(|p| p.into_inner());
            let reply = replies
                .get(*n)
                .or_else(|| replies.last())
                .cloned()
                .unwrap_or_else(|| Reply::status(500));
            *n += 1;
            reply
        })
        .await
    }

    async fn launch(
        handler: Handler,
        tls: Option<Arc<rustls::ServerConfig>>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let is_tls = tls.is_some();
        let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = handler.clone();
                let log = log.clone();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor {
                        None => {
                            let _ = serve(stream, false, handler, log).await;
                        }
                        Some(acceptor) => {
                            if let Ok(tls) = acceptor.accept(stream).await {
                                let cert = tls.get_ref().1.peer_certificates().is_some();
                                let _ = serve(tls, cert, handler, log).await;
                            }
                        }
                    }
                });
            }
        });
        Ok(MockServer {
            addr,
            tls: is_tls,
            requests,
            accept,
        })
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://127.0.0.1:<port><path>` (or `https://`).
    pub fn url(&self, path: &str) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}{path}", self.addr)
    }

    /// Every request so far, oldest first.
    pub fn requests(&self) -> Vec<Captured> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// The last request.
    pub fn last_request(&self) -> Option<Captured> {
        self.requests().pop()
    }
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_owned())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn fill<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    need: usize,
) -> std::io::Result<()> {
    let mut tmp = [0u8; 8192];
    while buf.len() < need {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(invalid("connection closed early"));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Ok(())
}

/// Reads a request head; returns it and any bytes read past it.
async fn read_head<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> std::io::Result<(Captured, Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::new();
    let end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(invalid("head too large"));
        }
        let want = buf.len() + 1;
        fill(stream, &mut buf, want).await?;
    };
    let raw_head = buf[..end + 4].to_vec();
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| invalid("head not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let mut captured = Captured {
        method: first.next().unwrap_or("").to_owned(),
        target: first.next().unwrap_or("").to_owned(),
        ..Captured::default()
    };
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(|| invalid("bad header"))?;
        captured
            .headers
            .push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let rest = buf.split_off(end + 4);
    Ok((captured, raw_head, rest))
}

/// Reads the body that follows a head (Content-Length or chunked).
async fn read_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    captured: &mut Captured,
    mut rest: Vec<u8>,
) -> std::io::Result<()> {
    let chunked = captured
        .header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    if chunked {
        loop {
            let line_end = loop {
                if let Some(i) = find(&rest, b"\r\n") {
                    break i;
                }
                let want = rest.len() + 1;
                fill(stream, &mut rest, want).await?;
            };
            let size_text = std::str::from_utf8(&rest[..line_end]).map_err(|_| invalid("size"))?;
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| invalid("size"))?;
            let need = (line_end + 4)
                .checked_add(size)
                .ok_or_else(|| invalid("size"))?;
            fill(stream, &mut rest, need).await?;
            captured
                .body
                .extend_from_slice(&rest[line_end + 2..line_end + 2 + size]);
            rest.drain(..need);
            if size == 0 {
                return Ok(());
            }
        }
    }
    if let Some(length) = captured.header("content-length") {
        let length: usize = length.parse().map_err(|_| invalid("length"))?;
        fill(stream, &mut rest, length).await?;
        rest.truncate(length);
        captured.body = rest;
    }
    Ok(())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        407 => "Proxy Authentication Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    client_cert: bool,
    handler: Handler,
    log: Arc<Mutex<Vec<Captured>>>,
) -> std::io::Result<()> {
    let (mut captured, _, rest) = read_head(&mut stream).await?;
    read_body(&mut stream, &mut captured, rest).await?;
    captured.client_cert = client_cert;
    let reply = handler(&captured);
    if let Ok(mut log) = log.lock() {
        log.push(captured);
    }
    if !reply.head_delay.is_zero() {
        tokio::time::sleep(reply.head_delay).await;
    }
    let mut head = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    match reply.framing {
        Framing::Chunked => head.push_str("Transfer-Encoding: chunked\r\n"),
        Framing::Length => head.push_str(&format!("Content-Length: {}\r\n", reply.body.len())),
        Framing::Close => {}
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    let limit = reply
        .cut_after
        .unwrap_or(reply.body.len())
        .min(reply.body.len());
    let body = &reply.body[..limit];
    let piece = if reply.piece == 0 {
        body.len().max(1)
    } else {
        reply.piece
    };
    for part in body.chunks(piece) {
        if !reply.delay.is_zero() {
            tokio::time::sleep(reply.delay).await;
        }
        if reply.framing == Framing::Chunked {
            stream
                .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                .await?;
            stream.write_all(part).await?;
            stream.write_all(b"\r\n").await?;
        } else {
            stream.write_all(part).await?;
        }
        stream.flush().await?;
    }
    if reply.cut_after.is_none() && reply.framing == Framing::Chunked {
        stream.write_all(b"0\r\n\r\n").await?;
    }
    stream.flush().await?;
    stream.shutdown().await
}

/// A forward proxy for tests.
pub struct MockProxy {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Captured>>>,
    accept: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for MockProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockProxy")
            .field("addr", &self.addr)
            .finish()
    }
}

impl Drop for MockProxy {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl MockProxy {
    /// Starts a proxy. With `credentials` (`user:pass`), requests without the
    /// matching `Proxy-Authorization: Basic` get `407`.
    ///
    /// # Errors
    ///
    /// Binding failed.
    pub async fn start(credentials: Option<&str>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let expected = credentials.map(|c| format!("Basic {}", base64(c.as_bytes())));
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let log = log.clone();
                let expected = expected.clone();
                tokio::spawn(async move {
                    let _ = proxy(stream, expected, log).await;
                });
            }
        });
        Ok(MockProxy { addr, seen, accept })
    }

    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every request head the proxy received.
    pub fn seen(&self) -> Vec<Captured> {
        self.seen.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

async fn proxy(
    mut client: TcpStream,
    expected: Option<String>,
    log: Arc<Mutex<Vec<Captured>>>,
) -> std::io::Result<()> {
    let (captured, raw_head, rest) = read_head(&mut client).await?;
    let authorised = match &expected {
        None => true,
        Some(value) => captured.header("proxy-authorization") == Some(value.as_str()),
    };
    let method = captured.method.clone();
    let target = captured.target.clone();
    if let Ok(mut log) = log.lock() {
        log.push(captured);
    }
    if !authorised {
        client
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        return client.shutdown().await;
    }
    if method == "CONNECT" {
        let mut upstream = TcpStream::connect(target.as_str()).await?;
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        upstream.write_all(&rest).await?;
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        return Ok(());
    }
    // Absolute-form: `http://host:port/path`.
    let authority = target
        .strip_prefix("http://")
        .and_then(|t| t.split('/').next())
        .ok_or_else(|| invalid("not absolute-form"))?;
    let mut upstream = TcpStream::connect(authority).await?;
    upstream.write_all(&raw_head).await?;
    upstream.write_all(&rest).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Standard base64, for `Proxy-Authorization` checks.
pub fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for g in input.chunks(3) {
        let b = [g[0], *g.get(1).unwrap_or(&0), *g.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for k in 0..4 {
            if k <= g.len() {
                out.push(char::from(T[((n >> (18 - 6 * k)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The test PKI in `tests/data/tls/` (a CA, a server certificate for
/// `127.0.0.1` and `localhost`, a client certificate, and an unrelated CA).
pub mod tls {
    use std::sync::Arc;

    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    fn path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/data/tls")
            .join(name)
    }

    /// The bytes of a PKI file (`ca.pem`, `ca.der`, `client.pem`,
    /// `client.key`, `other-ca.pem`, ...).
    ///
    /// # Panics
    ///
    /// When the file is missing.
    pub fn file(name: &str) -> Vec<u8> {
        std::fs::read(path(name)).unwrap_or_else(|e| panic!("tests/data/tls/{name}: {e}"))
    }

    /// A server configuration with the test server certificate. With
    /// `require_client_cert`, clients must present a certificate the test
    /// CA issued.
    ///
    /// # Panics
    ///
    /// When the PKI files are missing or invalid.
    pub fn server_config(require_client_cert: bool) -> Arc<rustls::ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certs: Vec<CertificateDer<'static>> =
            CertificateDer::pem_slice_iter(&file("server.pem"))
                .collect::<Result<_, _>>()
                .unwrap_or_else(|e| panic!("server.pem: {e:?}"));
        let key = PrivateKeyDer::from_pem_slice(&file("server.key"))
            .unwrap_or_else(|e| panic!("server.key: {e:?}"));
        let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap_or_else(|e| panic!("{e}"));
        let builder = if require_client_cert {
            let mut roots = rustls::RootCertStore::empty();
            for c in CertificateDer::pem_slice_iter(&file("ca.pem")) {
                roots
                    .add(c.unwrap_or_else(|e| panic!("ca.pem: {e:?}")))
                    .unwrap_or_else(|e| panic!("{e}"));
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                provider,
            )
            .build()
            .unwrap_or_else(|e| panic!("{e}"));
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };
        Arc::new(
            builder
                .with_single_cert(certs, key)
                .unwrap_or_else(|e| panic!("{e}")),
        )
    }
}

/// A multi-threaded tokio runtime with I/O and timers, for tests and benches
/// that drive the HTTP client (hyper-util spawns its connection tasks).
///
/// # Panics
///
/// When the runtime cannot be built.
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap_or_else(|e| panic!("tokio runtime: {e}"))
}

/// A TCP listener that accepts connections and never answers: a peer that
/// hangs in the TLS handshake or before the response head.
pub struct SilentServer {
    addr: SocketAddr,
    accept: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for SilentServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SilentServer")
            .field("addr", &self.addr)
            .finish()
    }
}

impl Drop for SilentServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl SilentServer {
    /// Starts one.
    ///
    /// # Errors
    ///
    /// Binding failed.
    pub async fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let accept = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        Ok(SilentServer { addr, accept })
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_chunked(raw: &[u8]) -> std::io::Result<Vec<u8>> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap_or_else(|e| panic!("runtime: {e}"));
        let mut captured = Captured {
            headers: vec![("Transfer-Encoding".into(), "chunked".into())],
            ..Captured::default()
        };
        let mut stream = raw;
        rt.block_on(read_body(&mut stream, &mut captured, Vec::new()))?;
        Ok(captured.body)
    }

    #[test]
    fn a_chunked_body_is_dechunked() {
        let body = read_chunked(b"3\r\nabc\r\n0\r\n\r\n").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(body, b"abc");
    }

    #[test]
    fn a_chunk_size_that_overflows_is_invalid_data() {
        let e = read_chunked(b"ffffffffffffffff\r\nabc\r\n")
            .map(|_| ())
            .expect_err("an overflowing chunk size must be refused");
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    }
}
