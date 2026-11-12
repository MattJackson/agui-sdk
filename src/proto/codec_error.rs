//! The codec's internal error, converted to [`crate::Error`] at the module
//! boundary.

use std::fmt;

/// Result alias.
pub(super) type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong encoding, decoding or framing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(super) enum Error {
    // ---- encoding (JSON side) ----
    /// The input is not valid JSON.
    Json {
        /// Byte offset of the problem.
        offset: usize,
        /// What was wrong.
        reason: &'static str,
    },
    /// The event is not a JSON object.
    NotAnObject,
    /// The event has no string `type` member.
    MissingType,
    /// The event `type` is not one this build knows.
    UnknownEventType(String),
    /// A discriminated union member (`InputContent`, `InputContentSource`)
    /// has a missing or unknown `type`.
    UnknownVariant {
        /// Proto message name of the union.
        message: &'static str,
        /// The `type` found (empty when missing).
        value: String,
    },
    /// A member has the wrong JSON kind for its wire field.
    TypeMismatch {
        /// Proto field name.
        field: &'static str,
        /// The JSON kind expected.
        expected: &'static str,
        /// The JSON kind found.
        found: &'static str,
    },
    /// An enum member has a value with no wire number.
    UnknownEnumValue {
        /// Proto field name.
        field: &'static str,
        /// The value found.
        value: String,
    },
    /// An `int64` member is not an integer in the JS safe-integer range.
    IntegerOutOfRange {
        /// Proto field name.
        field: &'static str,
    },
    /// Nesting deeper than the codec's bound ([`super::json::MAX_DEPTH`]
    /// encoding, `decoder::DECODE_MAX_DEPTH` decoding).
    DepthLimit,
    /// More container entries (object members plus array elements) in one
    /// decoded event than the caller's budget.
    EntryLimit {
        /// The budget.
        max: usize,
    },
    /// Input or output larger than the 4 GiB the wire can describe.
    TooLarge,
    /// A message to decode is `u32::MAX` bytes or more (4 GiB less one byte):
    /// more than the decoder's `u32` positions can describe.
    InputTooLarge,

    // ---- decoding (protobuf side) ----
    /// A field runs past the end of its message.
    Truncated,
    /// A varint is longer than 10 bytes or overflows 64 bits.
    InvalidVarint,
    /// A tag carries a wire type that does not exist (6, 7) or an unmatched
    /// end-group.
    InvalidWireType(u8),
    /// A tag carries field number 0 or one above 2^29 - 1.
    InvalidFieldNumber,
    /// A string field is not valid UTF-8.
    InvalidUtf8,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Json { offset, reason } => write!(f, "invalid JSON at byte {offset}: {reason}"),
            Error::NotAnObject => f.write_str("event is not a JSON object"),
            Error::MissingType => f.write_str("event has no string `type`"),
            Error::UnknownEventType(t) => write!(f, "unknown event type {t:?}"),
            Error::UnknownVariant { message, value } => {
                write!(f, "{message}: unknown or missing `type` {value:?}")
            }
            Error::TypeMismatch {
                field,
                expected,
                found,
            } => write!(f, "field `{field}`: expected {expected}, found {found}"),
            Error::UnknownEnumValue { field, value } => {
                write!(f, "field `{field}`: unknown value {value:?}")
            }
            Error::IntegerOutOfRange { field } => {
                write!(
                    f,
                    "field `{field}`: not an integer in the safe-integer range"
                )
            }
            Error::DepthLimit => f.write_str("nesting deeper than the protobuf codec allows"),
            Error::EntryLimit { max } => write!(f, "more than {max} container entries"),
            Error::TooLarge => f.write_str("message too large"),
            Error::InputTooLarge => f.write_str("protobuf message of u32::MAX bytes or more"),
            Error::Truncated => f.write_str("protobuf message truncated"),
            Error::InvalidVarint => f.write_str("malformed varint"),
            Error::InvalidWireType(t) => write!(f, "invalid wire type {t}"),
            Error::InvalidFieldNumber => f.write_str("invalid field number"),
            Error::InvalidUtf8 => f.write_str("string field is not valid UTF-8"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for crate::Error {
    fn from(e: Error) -> Self {
        use crate::ErrorCode;
        let code = match e {
            Error::Json { .. } => ErrorCode::Json,
            Error::NotAnObject
            | Error::MissingType
            | Error::UnknownEventType(_)
            | Error::UnknownVariant { .. }
            | Error::TypeMismatch { .. }
            | Error::UnknownEnumValue { .. } => ErrorCode::MalformedEvent,
            Error::IntegerOutOfRange { .. } | Error::TooLarge => ErrorCode::Encode,
            // The `u32` bound `framing` refuses at a frame header, one byte
            // tighter (`u32::MAX` itself marks a dropped `Struct` entry).
            Error::InputTooLarge => ErrorCode::FrameTooLarge,
            // Nesting is bounded in both directions; on the decoding side the
            // bytes are not a message this build can represent (PB-18).
            Error::DepthLimit | Error::EntryLimit { .. } => ErrorCode::LimitExceeded,
            Error::Truncated
            | Error::InvalidVarint
            | Error::InvalidWireType(_)
            | Error::InvalidFieldNumber
            | Error::InvalidUtf8 => ErrorCode::ProtoFraming,
        };
        crate::Error::new(code, e.to_string())
    }
}
