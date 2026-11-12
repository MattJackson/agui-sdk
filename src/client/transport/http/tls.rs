//! TLS client configuration from PEM/DER bytes, for rustls and native-tls.
//! No TLS library type leaves this module's crate-private API.

#![cfg_attr(
    not(any(feature = "rustls", feature = "native-tls")),
    allow(unused_imports)
)]

use crate::error::{Error, ErrorCode, Result};

use super::{TlsBackend, TlsRoots};

/// What the OS root store contributed under `TlsRoots::Native`, for `Debug`:
/// an unreadable store otherwise shows up only as unknown-issuer failures.
// The fields are read through `Debug` only, which the dead-code lint ignores.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct NativeRoots {
    /// Certificates added to the trust store.
    pub(crate) loaded: usize,
    /// Certificates found but not parsable.
    pub(crate) unparsable: usize,
    /// Errors raised while reading the store.
    pub(crate) load_errors: usize,
}

/// Everything the builder collected about TLS.
pub(crate) struct TlsSettings<'a> {
    pub(crate) roots: TlsRoots,
    pub(crate) der: &'a [Vec<u8>],
    pub(crate) pem: &'a [Vec<u8>],
    pub(crate) identity: Option<&'a (Vec<u8>, Vec<u8>)>,
}

impl TlsSettings<'_> {
    /// Whether anything beyond the defaults was asked for.
    pub(crate) fn customised(&self) -> bool {
        self.roots != TlsRoots::Bundled
            || !self.der.is_empty()
            || !self.pem.is_empty()
            || self.identity.is_some()
    }

    #[cfg_attr(not(any(feature = "rustls", feature = "native-tls")), allow(dead_code))]
    fn has_custom_roots(&self) -> bool {
        !self.der.is_empty() || !self.pem.is_empty()
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidConfig, message)
}

/// The backend to use: the requested one if compiled in, else rustls when
/// present, else native-tls; `None` when no TLS feature is enabled.
pub(crate) fn select_backend(requested: Option<TlsBackend>) -> Result<Option<TlsBackend>> {
    match requested {
        Some(TlsBackend::Rustls) if !cfg!(feature = "rustls") => {
            Err(invalid("TlsBackend::Rustls needs the `rustls` feature"))
        }
        Some(TlsBackend::NativeTls) if !cfg!(feature = "native-tls") => Err(invalid(
            "TlsBackend::NativeTls needs the `native-tls` feature",
        )),
        Some(backend) => Ok(Some(backend)),
        None if cfg!(feature = "rustls") => Ok(Some(TlsBackend::Rustls)),
        None if cfg!(feature = "native-tls") => Ok(Some(TlsBackend::NativeTls)),
        None => Ok(None),
    }
}

/// A rustls client configuration (ring provider, TLS 1.2 and 1.3).
#[cfg(feature = "rustls")]
pub(crate) fn rustls_config(
    settings: &TlsSettings<'_>,
) -> Result<(rustls::ClientConfig, Option<NativeRoots>)> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let mut store = rustls::RootCertStore::empty();
    #[cfg_attr(not(feature = "rustls-native-roots"), allow(unused_mut))]
    let mut native = None;
    match settings.roots {
        TlsRoots::Bundled => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        TlsRoots::Native => {
            #[cfg(feature = "rustls-native-roots")]
            {
                let found = rustls_native_certs::load_native_certs();
                let (loaded, unparsable) = store.add_parsable_certificates(found.certs);
                native = Some(NativeRoots {
                    loaded,
                    unparsable,
                    load_errors: found.errors.len(),
                });
                if store.is_empty() && !settings.has_custom_roots() {
                    return Err(invalid("no usable certificate in the OS root store"));
                }
            }
            #[cfg(not(feature = "rustls-native-roots"))]
            return Err(invalid(
                "TlsRoots::Native with rustls needs the `rustls-native-roots` feature",
            ));
        }
        TlsRoots::CustomOnly if !settings.has_custom_roots() => {
            return Err(invalid(
                "TlsRoots::CustomOnly needs at least one added root certificate",
            ));
        }
        _ => {}
    }
    for der in settings.der {
        store
            .add(CertificateDer::from(der.clone()))
            .map_err(|e| invalid("a DER root certificate is not valid").with_source(e))?;
    }
    for pem in settings.pem {
        let mut found = 0;
        for cert in CertificateDer::pem_slice_iter(pem) {
            let cert =
                cert.map_err(|_| invalid("a PEM root certificate bundle is not valid PEM"))?;
            store
                .add(cert)
                .map_err(|e| invalid("a PEM root certificate is not valid").with_source(e))?;
            found += 1;
        }
        if found == 0 {
            return Err(invalid("a PEM root bundle holds no certificate"));
        }
    }
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| invalid("the TLS provider rejected the protocol versions").with_source(e))?
        .with_root_certificates(store);
    let config = match settings.identity {
        Some((chain, key)) => {
            let certs = CertificateDer::pem_slice_iter(chain)
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| invalid("the client certificate chain is not valid PEM"))?;
            if certs.is_empty() {
                return Err(invalid("the client certificate chain holds no certificate"));
            }
            let key = PrivateKeyDer::from_pem_slice(key).map_err(|_| {
                invalid("the client private key is not a PEM PKCS #8, PKCS #1 or SEC1 key")
            })?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| invalid("the client identity is not usable").with_source(e))?
        }
        None => builder.with_no_client_auth(),
    };
    // No ALPN: HTTP/1.1 is the only protocol, and hyper-rustls sets none
    // for an HTTP/1-only connector.
    Ok((config, native))
}

/// A native-tls connector. The platform store is always the bundled one;
/// `CustomOnly` disables it. Client keys must be PKCS #8 here.
#[cfg(feature = "native-tls")]
pub(crate) fn native_connector(settings: &TlsSettings<'_>) -> Result<native_tls::TlsConnector> {
    let mut builder = native_tls::TlsConnector::builder();
    // TLS 1.2 at least, as the rustls backend's
    // `with_safe_default_protocol_versions`, whatever the platform would
    // otherwise allow (native-tls defaults to TLS 1.0; docs/DECISIONS.md D19).
    builder.min_protocol_version(Some(native_tls::Protocol::Tlsv12));
    if settings.roots == TlsRoots::CustomOnly {
        if !settings.has_custom_roots() {
            return Err(invalid(
                "TlsRoots::CustomOnly needs at least one added root certificate",
            ));
        }
        builder.disable_built_in_roots(true);
    }
    for der in settings.der {
        let cert = native_tls::Certificate::from_der(der)
            .map_err(|e| invalid("a DER root certificate is not valid").with_source(e))?;
        builder.add_root_certificate(cert);
    }
    for pem in settings.pem {
        // A cut-off last certificate is refused, as the rustls backend
        // refuses it, rather than trusting the bundle short of that root.
        let blocks = pem_blocks(pem, "CERTIFICATE")
            .ok_or_else(|| invalid("a PEM root certificate bundle is not valid PEM"))?;
        if blocks.is_empty() {
            return Err(invalid("a PEM root bundle holds no certificate"));
        }
        for block in blocks {
            let cert = native_tls::Certificate::from_pem(block)
                .map_err(|e| invalid("a PEM root certificate is not valid").with_source(e))?;
            builder.add_root_certificate(cert);
        }
    }
    if let Some((chain, key)) = settings.identity {
        let identity = native_tls::Identity::from_pkcs8(chain, key).map_err(|e| {
            invalid("the client identity is not usable (native-tls needs a PKCS #8 key)")
                .with_source(e)
        })?;
        builder.identity(identity);
    }
    builder
        .build()
        .map_err(|e| invalid("the platform TLS stack rejected the configuration").with_source(e))
}

/// The `-----BEGIN <label>-----` … `-----END <label>-----` blocks of a PEM
/// bundle, markers included. `None` when a `BEGIN` has no matching `END`.
#[cfg_attr(not(feature = "native-tls"), allow(dead_code))]
pub(crate) fn pem_blocks<'a>(pem: &'a [u8], label: &str) -> Option<Vec<&'a [u8]>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut blocks = Vec::new();
    let mut rest = pem;
    let mut offset = 0;
    while let Some(start) = find(rest, begin.as_bytes()) {
        let stop = find(&rest[start..], end.as_bytes())?;
        let stop = start + stop + end.len();
        blocks.push(&pem[offset + start..offset + stop]);
        offset += stop;
        rest = &pem[offset..];
    }
    Some(blocks)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(haystack, needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_pem_bundles() {
        let pem = b"junk\n-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n\
                    -----BEGIN CERTIFICATE-----\nBB==\n-----END CERTIFICATE-----\n";
        let blocks = pem_blocks(pem, "CERTIFICATE").unwrap_or_default();
        assert_eq!(blocks.len(), 2);
        assert!(blocks[1].starts_with(b"-----BEGIN CERTIFICATE-----\nBB=="));
        assert!(blocks[1].ends_with(b"-----END CERTIFICATE-----"));
        assert_eq!(pem_blocks(b"none", "CERTIFICATE"), Some(Vec::new()));
        // A last block cut off before its END marker is not silently skipped.
        let mut cut = pem.to_vec();
        cut.extend_from_slice(b"-----BEGIN CERTIFICATE-----\nCC==\n");
        assert_eq!(pem_blocks(&cut, "CERTIFICATE"), None);
    }
}
