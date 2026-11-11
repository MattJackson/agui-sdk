//! Protocol version handling (`basic/versioning.mdx`).
//!
//! Versions are `MAJOR.MINOR`, compared numerically per component (`1.10` >
//! `1.9`, VER-20). Pre-versioning 0.0.x peers are identified by configuration,
//! not by a declaration (they predate the field, VER-19), and select an era
//! downgrade ([`LegacyEra`]). When the peer's version is unknown the SDK
//! behaves as though it were current (VER-31).
//!
//! Nothing here allocates except [`PeerVersion::Uninterpretable`], which keeps
//! the declaration it could not read.

use std::fmt;

use crate::diagnostics::{Diagnostic, DiagnosticCode, Scope};

/// Parses one version component: ASCII digits, no sign, no leading zero
/// (except `0` itself), no overflow.
fn component(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    match bytes {
        [] => return None,
        [b'0', _, ..] => return None,
        _ => {}
    }
    let mut value: u64 = 0;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}

/// A parsed `MAJOR.MINOR` protocol version.
/// Ordering is numeric, major first, then minor (VER-20), as derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub struct ProtocolVersion {
    /// Major line.
    pub major: u64,
    /// Minor within the line.
    pub minor: u64,
}

impl ProtocolVersion {
    /// This crate's version, 1.0 (see [`crate::PROTOCOL_VERSION`]).
    pub const CURRENT: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

    /// `major.minor`.
    #[must_use]
    pub const fn new(major: u64, minor: u64) -> Self {
        ProtocolVersion { major, minor }
    }

    /// Parses `MAJOR.MINOR`. `None` for anything else, which callers treat as
    /// uninterpretable (VER-21).
    ///
    /// Each component is a decimal integer without sign or leading zeros, as
    /// the specification publishes them (`1.0`, `1.10`); surrounding
    /// whitespace, a third component (`1.0.0`) or a pre-release suffix make
    /// the text uninterpretable. Allocates nothing.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (major, minor) = text.split_once('.')?;
        Some(ProtocolVersion {
            major: component(major)?,
            minor: component(minor)?,
        })
    }
}

impl fmt::Display for ProtocolVersion {
    /// `MAJOR.MINOR`, the form [`ProtocolVersion::parse`] reads.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// How a peer's declared version relates to this crate's.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerVersion {
    /// No declaration: a pre-versioning peer, or one that chose not to say.
    /// Behave as current (VER-19, VER-31).
    Absent,
    /// Same version; silent (VER-27).
    Same,
    /// An older minor of the same line; silent.
    OlderMinor(ProtocolVersion),
    /// A newer minor of the same line: proceed and warn (VER-22, VER-23, VER-26).
    NewerMinor(ProtocolVersion),
    /// Not `MAJOR.MINOR`: treated like a newer version, proceed and warn (VER-21).
    Uninterpretable(String),
    /// A major line this crate does not implement. A server may reject before
    /// `RUN_STARTED` (VER-24); a client proceeds and warns.
    UnsupportedMajor(ProtocolVersion),
}

impl PeerVersion {
    /// Classifies a declaration against [`ProtocolVersion::CURRENT`].
    ///
    /// Components compare numerically (`1.10` is newer than `1.9`, VER-20).
    /// Any major other than the current one, older or newer, is
    /// [`PeerVersion::UnsupportedMajor`]. Allocates only for
    /// [`PeerVersion::Uninterpretable`].
    #[must_use]
    pub fn classify(declared: Option<&str>) -> Self {
        let Some(text) = declared else {
            return PeerVersion::Absent;
        };
        let Some(version) = ProtocolVersion::parse(text) else {
            return PeerVersion::Uninterpretable(text.to_owned());
        };
        let current = ProtocolVersion::CURRENT;
        if version.major != current.major {
            PeerVersion::UnsupportedMajor(version)
        } else if version.minor > current.minor {
            PeerVersion::NewerMinor(version)
        } else if version.minor < current.minor {
            PeerVersion::OlderMinor(version)
        } else {
            PeerVersion::Same
        }
    }

    /// The diagnostic this classification calls for, if any: a newer minor,
    /// an uninterpretable declaration and an unimplemented major warn; an
    /// absent, equal or older-minor declaration is silent (VER-27, VER-31).
    #[must_use]
    pub fn diagnostic_code(&self) -> Option<DiagnosticCode> {
        match self {
            PeerVersion::Absent | PeerVersion::Same | PeerVersion::OlderMinor(_) => None,
            PeerVersion::NewerMinor(_) => Some(DiagnosticCode::NewerMinorVersion),
            PeerVersion::Uninterpretable(_) => Some(DiagnosticCode::UninterpretableVersion),
            PeerVersion::UnsupportedMajor(_) => Some(DiagnosticCode::UnsupportedMajorVersion),
        }
    }

    /// The warning this classification calls for, pointing at
    /// `/protocolVersion` in `scope` (`Scope::Event` for a producer's
    /// `RUN_STARTED`, `Scope::RunInput` for a consumer's input). The caller
    /// adds the event type where there is one.
    ///
    /// The message names the declared version when it parsed. An
    /// uninterpretable declaration is described, not quoted, because it is
    /// arbitrary peer text. `None` exactly when
    /// [`diagnostic_code`](PeerVersion::diagnostic_code) is `None`.
    #[must_use]
    pub fn diagnostic(&self, scope: Scope) -> Option<Diagnostic> {
        let code = self.diagnostic_code()?;
        let current = ProtocolVersion::CURRENT;
        let message = match self {
            PeerVersion::NewerMinor(v) => format!(
                "peer declares protocol version {v}, newer than {current}; proceeding, \
                 and additions this SDK does not know are stripped. Upgrade agui-sdk \
                 to a release implementing {v}"
            ),
            PeerVersion::Uninterpretable(_) => format!(
                "cannot interpret the declared protocol version (not MAJOR.MINOR); \
                 treating it as newer than {current} and proceeding"
            ),
            PeerVersion::UnsupportedMajor(v) => format!(
                "peer declares protocol version {v}, a major line this SDK does not \
                 implement (it speaks {current}); proceeding under the processing model"
            ),
            PeerVersion::Absent | PeerVersion::Same | PeerVersion::OlderMinor(_) => return None,
        };
        Some(Diagnostic::new(code, scope, "/protocolVersion", message))
    }
}

/// A pre-versioning peer era, selected by configuration (VER-03, VER-30).
///
/// Pinning a peer to an era makes the client omit `protocolVersion` (the peer
/// predates the field, RUNIN-08) and installs that era's downgrade and upgrade
/// shims next to the wire, before user middleware (VER-14). Lossy shims warn
/// (VER-07); shims never invent values, repair malformed ones, change
/// outcomes, alter identifiers or reorder events (VER-04, VER-05, VER-32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum LegacyEra {
    /// Before 0.0.40: tool results and user content are plain strings
    /// (content parts are flattened, lossy).
    V0_0_39,
    /// Before 0.0.48: binary content in the legacy shape (upgraded on receipt).
    V0_0_47,
    /// Before 0.0.58: no subagents (subagent events are dropped, lossy).
    V0_0_57,
}

impl LegacyEra {
    /// Parses a `0.0.N` pin. Pins at or above 0.0.58 return `None` (no shim needed).
    ///
    /// `N` is a decimal integer without sign or leading zeros. Anything that
    /// is not `0.0.N` (a current `MAJOR.MINOR` version, garbage, the empty
    /// string) also returns `None`: without a recognised pin the peer is
    /// treated as current (VER-31). Allocates nothing.
    #[must_use]
    pub fn parse(version: &str) -> Option<Self> {
        let patch = component(version.strip_prefix("0.0.")?)?;
        match patch {
            0..=39 => Some(LegacyEra::V0_0_39),
            40..=47 => Some(LegacyEra::V0_0_47),
            48..=57 => Some(LegacyEra::V0_0_57),
            _ => None,
        }
    }

    /// The newest `0.0.N` release the era covers, e.g. `"0.0.57"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LegacyEra::V0_0_39 => "0.0.39",
            LegacyEra::V0_0_47 => "0.0.47",
            LegacyEra::V0_0_57 => "0.0.57",
        }
    }
}

/// What a client is told about its peer's protocol version, from
/// [`ClientBuilder::peer_version`][crate::client::ClientBuilder::peer_version].
/// Unset, a peer is treated as current (VER-31).
#[cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::ClientBuilder::peer_version]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.ClientBuilder.html#method.peer_version"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum PeerPin {
    /// Current: declare [`crate::PROTOCOL_VERSION`], no shims.
    #[default]
    Current,
    /// A known versioned peer (`MAJOR.MINOR`); declare ours, warn as
    /// [`PeerVersion`] classifies it.
    Version(ProtocolVersion),
    /// A pre-versioning peer: omit `protocolVersion` and install that era's
    /// shims (RUNIN-08, VER-03, VER-30).
    Legacy(LegacyEra),
}
