//! Fetching capability declarations over HTTP.
//!
//! The spec leaves discovery to the application (capabilities.mdx: fetched
//! from an application-defined endpoint, or configured statically). A
//! [`CapabilitiesRequest`] describes the GET;
//! [`HttpTransport::fetch_capabilities`](super::HttpTransport::fetch_capabilities)
//! performs it over the transport's own connection pool, TLS, proxy and CA
//! settings, and validates the declaration with
//! [`capability::ingest`](crate::capability::ingest). On any failure the
//! caller should proceed as for an agent that declared nothing (CAP-16).
//! The transport's own headers and credentials go only to the agent URL's
//! origin; a declaration elsewhere gets only the request's own headers.

use std::time::Duration;

/// Default largest declaration accepted: 1 MiB.
pub const DEFAULT_MAX_BYTES: usize = 1024 * 1024;

/// Default request timeout: 30 s.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A capability fetch to perform with
/// [`HttpTransport::fetch_capabilities`](super::HttpTransport::fetch_capabilities).
#[derive(Clone)]
pub struct CapabilitiesRequest {
    url: String,
    headers: Vec<(String, String)>,
    timeout: Duration,
    max_bytes: usize,
}

impl CapabilitiesRequest {
    /// A GET of `url`.
    pub fn new(url: impl Into<String>) -> Self {
        CapabilitiesRequest {
            url: url.into(),
            headers: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Adds a header. A header of the same name configured on the transport
    /// is replaced, not duplicated.
    #[must_use]
    pub fn header(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let (name, value) = (name.into(), value.into());
        let mut request = self;
        request.headers.push((name, value));
        request
    }

    /// `Authorization: Bearer <token>`, redacted from `Debug`.
    #[must_use]
    pub fn bearer_token(self, token: impl AsRef<str>) -> Self {
        let mut credential = String::from("Bearer ");
        credential.push_str(token.as_ref());
        self.header("Authorization", credential)
    }

    /// Request timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Body size limit.
    #[must_use]
    pub fn max_bytes(mut self, bytes: usize) -> Self {
        self.max_bytes = bytes;
        self
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub(crate) fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn max_bytes_value(&self) -> usize {
        self.max_bytes
    }
}

impl std::fmt::Debug for CapabilitiesRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<super::transport::Header> = self
            .headers
            .iter()
            .map(|(n, v)| super::transport::Header::new(n.clone(), v.clone()))
            .collect();
        f.debug_struct("CapabilitiesRequest")
            .field("url", &super::transport::redact_url(&self.url))
            .field("headers", &headers)
            .field("timeout", &self.timeout)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}
