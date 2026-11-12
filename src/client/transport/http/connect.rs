//! The connector under hyper-util's pooled client: TCP (hyper-util's
//! `HttpConnector`), an optional HTTP proxy (absolute-form for `http://`
//! targets, a `CONNECT` tunnel for `https://`), TLS per feature, and one
//! connect timeout covering all of it.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use ::http::{HeaderValue, Uri};
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tower_service::Service;

use super::endpoint::{self, Endpoint};
use crate::error::{BoxError, Error, ErrorCode, Result};

/// Largest proxy `CONNECT` response head read.
const MAX_TUNNEL_HEAD: usize = 8 * 1024;

/// One proxy server.
#[derive(Clone)]
pub(crate) struct ProxyServer {
    /// `http://host:port/`, what the TCP connector dials.
    pub(crate) uri: Uri,
    /// `Proxy-Authorization` from the proxy URL's userinfo (sensitive).
    pub(crate) auth: Option<HeaderValue>,
    /// The proxy URL for `Debug`, credentials redacted.
    pub(crate) display: String,
}

impl ProxyServer {
    /// Parses `http://[user:pass@]host:port`; a bare `host:port` (common in
    /// environment variables) is read as `http://`.
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let owned;
        let text = if text.contains("://") {
            text
        } else {
            owned = format!("http://{text}");
            &owned
        };
        let parsed = Endpoint::parse(text, ErrorCode::InvalidConfig, "the proxy URL")?;
        if endpoint::is_https(&parsed.uri) {
            return Err(Error::new(
                ErrorCode::InvalidConfig,
                "the proxy URL must be http:// (TLS to the proxy is not supported)",
            ));
        }
        let auth = match &parsed.userinfo {
            Some((user, pass)) => Some(endpoint::basic_auth(user, pass).ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidConfig,
                    "proxy credentials are not a valid header",
                )
            })?),
            None => None,
        };
        let authority = parsed.uri.authority().map(|a| a.as_str()).unwrap_or("");
        let uri = Uri::try_from(format!("http://{authority}/"))
            .map_err(|_| Error::new(ErrorCode::InvalidConfig, "the proxy URL is not valid"))?;
        Ok(ProxyServer {
            uri,
            auth,
            display: endpoint::redact_url(text),
        })
    }
}

/// Which proxy, if any, serves a target.
#[derive(Clone, Default)]
pub(crate) struct ProxyConfig {
    pub(crate) http: Option<ProxyServer>,
    pub(crate) https: Option<ProxyServer>,
    /// `NO_PROXY` entries, lowercased, leading dots removed; `*` is all.
    pub(crate) no_proxy: Vec<String>,
}

impl ProxyConfig {
    /// One explicit proxy for both schemes.
    pub(crate) fn explicit(server: ProxyServer) -> Self {
        ProxyConfig {
            http: Some(server.clone()),
            https: Some(server),
            no_proxy: Vec::new(),
        }
    }

    /// `https_proxy`/`HTTPS_PROXY`, `http_proxy`/`HTTP_PROXY` and
    /// `no_proxy`/`NO_PROXY`, lowercase first. Uppercase `HTTP_PROXY` is
    /// ignored under CGI (`REQUEST_METHOD` set), where a client could set it
    /// through a `Proxy:` request header ("httpoxy").
    pub(crate) fn from_env() -> Result<Self> {
        let present = |value: String| Some(value).filter(|v| !v.trim().is_empty());
        // A non-Unicode proxy variable is a configuration error (naming the
        // variable, never its value): it must not quietly mean "direct".
        let proxy_var = |name: &str| -> Result<Option<String>> {
            match std::env::var(name) {
                Ok(value) => Ok(present(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => Err(Error::new(
                    ErrorCode::InvalidConfig,
                    format!("proxy variable `{name}` is not valid Unicode"),
                )),
            }
        };
        let cgi = std::env::var_os("REQUEST_METHOD").is_some();
        let http = match proxy_var("http_proxy")? {
            Some(value) => Some(value),
            None if cgi => None,
            None => proxy_var("HTTP_PROXY")?,
        };
        let https = match proxy_var("https_proxy")? {
            Some(value) => Some(value),
            None => proxy_var("HTTPS_PROXY")?,
        };
        let no_proxy = match proxy_var("no_proxy")? {
            Some(value) => value,
            None => proxy_var("NO_PROXY")?.unwrap_or_default(),
        };
        Ok(ProxyConfig {
            http: http.as_deref().map(ProxyServer::parse).transpose()?,
            https: https.as_deref().map(ProxyServer::parse).transpose()?,
            no_proxy: no_proxy
                .split(',')
                .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
        })
    }

    /// The proxy for `uri`, unless `NO_PROXY` exempts its host.
    pub(crate) fn for_uri(&self, uri: &Uri) -> Option<&ProxyServer> {
        let server = if endpoint::is_https(uri) {
            self.https.as_ref()
        } else {
            self.http.as_ref()
        }?;
        let host = uri
            .host()
            .unwrap_or("")
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        let exempt = self.no_proxy.iter().any(|entry| {
            entry == "*"
                || host == *entry
                || (host.len() > entry.len()
                    && host.ends_with(entry.as_str())
                    && host.as_bytes()[host.len() - entry.len() - 1] == b'.')
        });
        (!exempt).then_some(server)
    }

    /// Whether any proxy is configured.
    pub(crate) fn is_empty(&self) -> bool {
        self.http.is_none() && self.https.is_none()
    }
}

/// A TCP connection, possibly to a proxy that expects absolute-form requests.
pub(crate) struct TcpConn {
    io: TokioIo<TcpStream>,
    proxied: bool,
}

impl Connection for TcpConn {
    fn connected(&self) -> Connected {
        self.io.connected().proxy(self.proxied)
    }
}

impl Read for TcpConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl Write for TcpConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }
}

type Connecting<T> = Pin<Box<dyn Future<Output = std::result::Result<T, BoxError>> + Send>>;

/// TCP to the target, or to its proxy (tunnelling `https://` targets).
#[derive(Clone)]
pub(crate) struct ProxyTcp {
    pub(crate) tcp: HttpConnector,
    pub(crate) proxy: Arc<ProxyConfig>,
}

impl Service<Uri> for ProxyTcp {
    type Response = TcpConn;
    type Error = BoxError;
    type Future = Connecting<TcpConn>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), BoxError>> {
        self.tcp.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, target: Uri) -> Self::Future {
        let Some(server) = self.proxy.for_uri(&target).cloned() else {
            let dialing = self.tcp.call(target);
            return Box::pin(async move {
                Ok(TcpConn {
                    io: dialing.await?,
                    proxied: false,
                })
            });
        };
        let dialing = self.tcp.call(server.uri.clone());
        Box::pin(async move {
            let io = dialing.await?;
            if !endpoint::is_https(&target) {
                return Ok(TcpConn { io, proxied: true });
            }
            let host = target.host().unwrap_or("");
            let port = target.port_u16().unwrap_or(443);
            let stream = tunnel(io.into_inner(), host, port, server.auth.as_ref()).await?;
            Ok(TcpConn {
                io: TokioIo::new(stream),
                proxied: false,
            })
        })
    }
}

/// The proxy refused or garbled a `CONNECT`.
#[derive(Debug)]
struct TunnelError(String);

impl std::fmt::Display for TunnelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TunnelError {}

/// Opens a `CONNECT host:port` tunnel through a proxy.
async fn tunnel(
    stream: TcpStream,
    host: &str,
    port: u16,
    auth: Option<&HeaderValue>,
) -> std::result::Result<TcpStream, BoxError> {
    let mut head = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(auth) = auth {
        head.push_str("Proxy-Authorization: ");
        head.push_str(auth.to_str().unwrap_or(""));
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let mut sent = 0;
    let bytes = head.as_bytes();
    while sent < bytes.len() {
        stream.writable().await?;
        match stream.try_write(&bytes[sent..]) {
            Ok(n) => sent += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
    }
    // Read byte-wise up to the end of the response head, so no tunnelled
    // byte is consumed here.
    let mut response = Vec::with_capacity(256);
    while !response.ends_with(b"\r\n\r\n") {
        if response.len() >= MAX_TUNNEL_HEAD {
            return Err(Box::new(TunnelError(
                "the proxy's CONNECT response head is too large".to_owned(),
            )));
        }
        stream.readable().await?;
        let mut byte = [0u8; 1];
        match stream.try_read(&mut byte) {
            Ok(0) => {
                return Err(Box::new(TunnelError(
                    "the proxy closed the connection during CONNECT".to_owned(),
                )))
            }
            Ok(_) => response.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
    }
    match tunnel_status(&response) {
        Some(200..=299) => Ok(stream),
        Some(code) => Err(Box::new(TunnelError(format!(
            "the proxy refused the CONNECT tunnel with HTTP {code}"
        )))),
        None => Err(Box::new(TunnelError(
            "the proxy sent an unreadable CONNECT response".to_owned(),
        ))),
    }
}

/// The status code of a CONNECT response head.
fn tunnel_status(head: &[u8]) -> Option<u16> {
    let line = head.split(|b| *b == b'\r' || *b == b'\n').next()?;
    line.split(u8::is_ascii_whitespace)
        .filter(|piece| !piece.is_empty())
        .nth(1)
        .and_then(|s| std::str::from_utf8(s).ok())
        .and_then(|s| s.parse::<u16>().ok())
}

#[cfg(test)]
mod tests {
    use super::{tunnel_status, ProxyConfig, ProxyServer};

    fn exempt(no_proxy: &[&str], target: &str) -> bool {
        let server = ProxyServer::parse("http://127.0.0.1:3128").unwrap_or_else(|e| panic!("{e}"));
        let config = ProxyConfig {
            http: Some(server),
            https: None,
            no_proxy: no_proxy.iter().map(|e| e.to_string()).collect(),
        };
        let uri = target.parse().unwrap_or_else(|e| panic!("{e}"));
        config.for_uri(&uri).is_none()
    }

    /// `NO_PROXY` entries (stored without a leading dot) exempt the host and
    /// its subdomains on a label boundary, never a host that merely ends in
    /// the same characters.
    #[test]
    fn no_proxy_matches_on_a_label_boundary() {
        assert!(exempt(&["example.com"], "http://example.com/"));
        assert!(exempt(&["example.com"], "http://sub.example.com/"));
        assert!(exempt(&["example.com"], "http://SUB.Example.COM/"));
        assert!(!exempt(&["example.com"], "http://evilexample.com/"));
        assert!(!exempt(&["example.com"], "http://example.com.evil/"));
        assert!(!exempt(&["sub.example.com"], "http://example.com/"));
        assert!(exempt(&["*"], "http://anything.test/"));
        assert!(exempt(&["::1"], "http://[::1]:8080/"));
        assert!(!exempt(&[], "http://example.com/"));
    }

    #[test]
    fn the_status_is_read_from_the_status_line_only() {
        assert_eq!(tunnel_status(b"HTTP/1.1 200\r\n\r\n"), Some(200));
        assert_eq!(
            tunnel_status(b"HTTP/1.1 200 Connection established\r\n\r\n"),
            Some(200)
        );
        assert_eq!(
            tunnel_status(b"HTTP/1.1 407 Proxy Authentication Required\r\nX: a 1\r\n\r\n"),
            Some(407)
        );
        assert_eq!(tunnel_status(b"garbage\r\n\r\n"), None);
        assert_eq!(tunnel_status(b"HTTP/1.1 abc\r\n\r\n"), None);
    }
}

/// The connect timeout elapsed (TCP, proxy tunnel and TLS handshake).
#[derive(Debug)]
pub(crate) struct ConnectTimeout(pub(crate) Duration);

impl std::fmt::Display for ConnectTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "connecting took longer than {:?}", self.0)
    }
}

impl std::error::Error for ConnectTimeout {}

/// An `https://` target with no TLS backend compiled in.
#[derive(Debug)]
pub(crate) struct NoTls;

impl std::fmt::Display for NoTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("https:// needs the `rustls` or `native-tls` feature")
    }
}

impl std::error::Error for NoTls {}

/// The TLS layer of the connector.
#[derive(Clone)]
pub(crate) enum Tls {
    /// Plain TCP only.
    None,
    /// rustls.
    #[cfg(feature = "rustls")]
    Rustls(hyper_rustls::HttpsConnector<ProxyTcp>),
    /// The platform stack.
    #[cfg(feature = "native-tls")]
    Native(hyper_tls::HttpsConnector<ProxyTcp>),
}

/// The client's connector: [`ProxyTcp`] under the selected TLS layer, all
/// within the connect timeout.
#[derive(Clone)]
pub(crate) struct Connector {
    pub(crate) tcp: ProxyTcp,
    pub(crate) tls: Tls,
    pub(crate) timeout: Duration,
}

/// A connection of any of the connector's kinds. Boxed: TLS state is large,
/// and this is one allocation per connection, not per request.
pub(crate) enum Conn {
    Plain(Box<TcpConn>),
    #[cfg(feature = "rustls")]
    Rustls(Box<hyper_rustls::MaybeHttpsStream<TcpConn>>),
    #[cfg(feature = "native-tls")]
    Native(Box<hyper_tls::MaybeHttpsStream<TcpConn>>),
}

macro_rules! each_conn {
    ($self:expr, $io:ident => $body:expr) => {
        match $self {
            Conn::Plain($io) => $body,
            #[cfg(feature = "rustls")]
            Conn::Rustls($io) => $body,
            #[cfg(feature = "native-tls")]
            Conn::Native($io) => $body,
        }
    };
}

impl Connection for Conn {
    fn connected(&self) -> Connected {
        each_conn!(self, io => io.connected())
    }
}

impl Read for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        each_conn!(self.get_mut(), io => Pin::new(&mut **io).poll_read(cx, buf))
    }
}

impl Write for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        each_conn!(self.get_mut(), io => Pin::new(&mut **io).poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        each_conn!(self.get_mut(), io => Pin::new(&mut **io).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        each_conn!(self.get_mut(), io => Pin::new(&mut **io).poll_shutdown(cx))
    }

    fn is_write_vectored(&self) -> bool {
        each_conn!(self, io => io.is_write_vectored())
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        each_conn!(self.get_mut(), io => Pin::new(&mut **io).poll_write_vectored(cx, bufs))
    }
}

impl Service<Uri> for Connector {
    type Response = Conn;
    type Error = BoxError;
    type Future = Connecting<Conn>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), BoxError>> {
        match &mut self.tls {
            Tls::None => self.tcp.poll_ready(cx),
            #[cfg(feature = "rustls")]
            Tls::Rustls(c) => c.poll_ready(cx),
            #[cfg(feature = "native-tls")]
            Tls::Native(c) => c.poll_ready(cx),
        }
    }

    fn call(&mut self, target: Uri) -> Self::Future {
        let connecting: Connecting<Conn> = match &mut self.tls {
            Tls::None if endpoint::is_https(&target) => {
                return Box::pin(async { Err(Box::new(NoTls) as BoxError) });
            }
            Tls::None => {
                let f = self.tcp.call(target);
                Box::pin(async move { f.await.map(|c| Conn::Plain(Box::new(c))) })
            }
            #[cfg(feature = "rustls")]
            Tls::Rustls(c) => {
                let f = c.call(target);
                Box::pin(async move { f.await.map(|c| Conn::Rustls(Box::new(c))) })
            }
            #[cfg(feature = "native-tls")]
            Tls::Native(c) => {
                let f = c.call(target);
                Box::pin(async move { f.await.map(|c| Conn::Native(Box::new(c))) })
            }
        };
        let limit = self.timeout;
        Box::pin(async move {
            match tokio::time::timeout(limit, connecting).await {
                Ok(result) => result,
                Err(_) => Err(Box::new(ConnectTimeout(limit)) as BoxError),
            }
        })
    }
}
