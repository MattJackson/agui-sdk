//! Admitting run input before any stream exists.
//!
//! [`admit`] parses the body once, strips unknown members with warnings
//! (RUNIN-30, PROC-35), rejects malformed input (RUNIN-29), classifies the
//! declared version (VER-21..VER-25). Absent collections stay absent in
//! [`AdmittedInput::input`]; they read as empty through
//! [`RunAgentInput::tools`], [`RunAgentInput::context`] and
//! [`RunAgentInput::resume`], and the runner treats an absent state as `{}`
//! (RUNIN-20, RUNIN-23, RUNIN-24). A rejection carries the HTTP status to use;
//! it is never turned into `RUN_ERROR` (SSE-12, PB-19, TRN-05).

use std::sync::Arc;

use crate::diagnostics::{CollectingSink, Diagnostic, Reporter, Scope, DEFAULT_MAX_PER_EVENT};
use crate::error::{Error, ErrorCode, Result};
use crate::pipeline::enforce::Enforcer;
use crate::pipeline::Limits;
use crate::run_input::RunAgentInput;
use crate::version::PeerVersion;

/// Default [`InputPolicy::max_body_bytes`], and the most the axum extractor
/// reads of any body.
pub(crate) const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Admission policy.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InputPolicy {
    /// Largest accepted body. Default 16 MiB (HTTP 413 beyond). The axum
    /// extractor never reads more than 16 MiB whatever this is set to (see
    /// the `server::axum` module); a larger value applies to
    /// bodies the application reads itself.
    pub max_body_bytes: usize,
    /// Reject a declaration from an unimplemented major line before
    /// `RUN_STARTED` (VER-24). Default on. Newer minors and uninterpretable
    /// declarations are always served (VER-21, VER-25).
    pub reject_unsupported_major: bool,
}

impl Default for InputPolicy {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            reject_unsupported_major: true,
        }
    }
}

/// Input that passed admission.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct AdmittedInput {
    /// The input, unknown members stripped.
    pub input: RunAgentInput,
    /// How the client's declared version relates to ours.
    pub peer_version: PeerVersion,
    /// What admission stripped or noted; reported to the runner's sink when
    /// the run starts.
    pub diagnostics: Vec<Diagnostic>,
}

/// Admits a request body.
///
/// # Errors
///
/// [`ErrorCode::InputRejected`] (HTTP 400),
/// [`ErrorCode::RequestTooLarge`] (413) or
/// [`ErrorCode::UnsupportedVersion`] (400).
pub fn admit(body: &[u8], policy: &InputPolicy) -> Result<AdmittedInput> {
    if body.len() > policy.max_body_bytes {
        return Err(Error::new(
            ErrorCode::RequestTooLarge,
            "the run input exceeds the size limit",
        )
        .with_http_status(413));
    }
    let collected = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(collected.clone(), DEFAULT_MAX_PER_EVENT);
    let input = Enforcer::new(Limits::default())
        .enforce_incoming_input(body, &mut reporter)
        .map_err(reject)?;
    reporter.end_event(None);
    let mut admitted = admit_input(input, policy)?;
    let mut diagnostics = collected.take();
    diagnostics.append(&mut admitted.diagnostics);
    admitted.diagnostics = diagnostics;
    Ok(admitted)
}

/// Malformed input is the transport's 400, never a `RUN_ERROR` (RUNIN-29).
fn reject(cause: Error) -> Error {
    let mut error =
        Error::new(ErrorCode::InputRejected, cause.message().to_owned()).with_http_status(400);
    if let Some(pointer) = cause.pointer() {
        error = error.with_pointer(pointer.to_owned());
    }
    error.with_source(cause)
}

/// Admits an already-typed input (e.g. from a non-HTTP transport), applying
/// the version policy only.
///
/// # Errors
///
/// [`ErrorCode::UnsupportedVersion`].
pub fn admit_input(input: RunAgentInput, policy: &InputPolicy) -> Result<AdmittedInput> {
    let peer_version = PeerVersion::classify(input.protocol_version.as_deref());
    if let PeerVersion::UnsupportedMajor(version) = &peer_version {
        if policy.reject_unsupported_major {
            // Only another major line is refused (VER-24); a newer minor of
            // ours is served (VER-22, VER-25).
            return Err(Error::new(
                ErrorCode::UnsupportedVersion,
                format!(
                    "protocol version {version} is not implemented; this server speaks {}",
                    crate::PROTOCOL_VERSION
                ),
            )
            .with_pointer("/protocolVersion")
            .with_http_status(400));
        }
    }
    let mut diagnostics = Vec::new();
    if let Some(code) = peer_version.diagnostic_code() {
        let message = match &peer_version {
            PeerVersion::NewerMinor(version) => format!(
                "client declares protocol {version}, newer than {}; serving it as {}",
                crate::PROTOCOL_VERSION,
                crate::PROTOCOL_VERSION
            ),
            PeerVersion::UnsupportedMajor(version) => format!(
                "client declares protocol {version}, a major line this server does not implement"
            ),
            // Arbitrary peer text is described, not quoted.
            _ => format!(
                "client's protocol version could not be read; serving it as {}",
                crate::PROTOCOL_VERSION
            ),
        };
        diagnostics.push(Diagnostic::new(
            code,
            Scope::RunInput,
            "/protocolVersion",
            message,
        ));
    }
    Ok(AdmittedInput {
        input,
        peer_version,
        diagnostics,
    })
}
