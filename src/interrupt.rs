//! Interrupts and resume (`patterns/interrupt-resume.mdx`).
//!
//! A run that needs outside input finishes with the interrupt outcome
//! (INTR-01). The next run on the thread answers every interrupt with a
//! [`ResumeEntry`]: resolved, or explicitly cancelled; omission is not
//! abandonment (INTR-10). The client checks coverage and expiry before sending
//! anything ([`Thread::validate_resume`][crate::client::Thread::validate_resume],
//! INTR-11, INTR-14, INTR-15).
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::Thread::validate_resume]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.Thread.html#method.validate_resume"
)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{InterruptId, SubagentRunId, ToolCallId};
use crate::json::JsonObject;
use crate::metadata::Metadata;

/// Answer status of a [`ResumeEntry`] (INTR-09).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ResumeStatus {
    /// The interrupt was answered; `payload` carries the answer.
    Resolved,
    /// The interrupt was abandoned. Allowed for an expired interrupt (INTR-15).
    Cancelled,
}

/// An interrupt's expiry, as read by the client (INTR-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Expiry {
    /// No `expiresAt`: the interrupt never expires.
    Never,
    /// Expires at this instant; an instant now or earlier is expired.
    At(SystemTime),
    /// `expiresAt` could not be read as an RFC 3339 timestamp. Treated as
    /// never expiring, with an `interrupt.unparseable_expiry` warning.
    Unparseable,
}

/// A request for outside input that ended a run (INTR-01). Carried by
/// [`RunFinishedOutcome::Interrupt`](crate::lifecycle::RunFinishedOutcome::Interrupt).
///
/// Schema: `$defs/Interrupt`. Spec rules: INTR-01..INTR-07, INTR-14.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Interrupt {
    /// Unique within the run (INTR-04).
    pub id: InterruptId,
    /// Open string naming why input is needed (INTR-05).
    pub reason: String,
    /// Human-readable prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Tool call the interrupt concerns, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// Opaque JSON Schema describing the expected payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<JsonObject>,
    /// Expiry timestamp as sent. Read through [`Interrupt::expiry`] (INTR-14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Stored annotations (META-16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Invocation that raised it; absent means the parent (INTR-06, SUB-35).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl Interrupt {
    /// Creates the value from its required fields (`id`, `reason`); every optional
    /// field starts absent.
    pub fn new(id: impl Into<InterruptId>, reason: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            reason: reason.into(),
            message: None,
            tool_call_id: None,
            response_schema: None,
            expires_at: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`message`](Self::message).
    #[must_use]
    pub fn with_message(self, message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            ..self
        }
    }

    /// Sets [`tool_call_id`](Self::tool_call_id).
    #[must_use]
    pub fn with_tool_call_id(self, tool_call_id: impl Into<ToolCallId>) -> Self {
        Self {
            tool_call_id: Some(tool_call_id.into()),
            ..self
        }
    }

    /// Sets [`response_schema`](Self::response_schema).
    #[must_use]
    pub fn with_response_schema(self, response_schema: JsonObject) -> Self {
        Self {
            response_schema: Some(response_schema),
            ..self
        }
    }

    /// Sets [`expires_at`](Self::expires_at).
    #[must_use]
    pub fn with_expires_at(self, expires_at: impl Into<String>) -> Self {
        Self {
            expires_at: Some(expires_at.into()),
            ..self
        }
    }

    /// Sets [`metadata`](Self::metadata).
    #[must_use]
    pub fn with_metadata(self, metadata: Metadata) -> Self {
        Self {
            metadata: Some(metadata),
            ..self
        }
    }

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}

/// Answer to one interrupt, sent on the next run's input (INTR-08, INTR-09).
///
/// Schema: `$defs/ResumeEntry`. Spec rules: INTR-08..INTR-12, INTR-15.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ResumeEntry {
    /// The interrupt being answered.
    pub interrupt_id: InterruptId,
    /// Resolved or cancelled (INTR-09).
    pub status: ResumeStatus,
    /// The answer, any non-null JSON value.
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub payload: Option<Value>,
    /// Envelope information (INTR-09).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl ResumeEntry {
    /// Creates the value from its required fields (`interrupt_id`, `status`); every optional
    /// field starts absent.
    pub fn new(interrupt_id: impl Into<InterruptId>, status: ResumeStatus) -> Self {
        Self {
            interrupt_id: interrupt_id.into(),
            status,
            payload: None,
            metadata: None,
        }
    }

    /// Sets [`payload`](Self::payload).
    #[must_use]
    pub fn with_payload(self, payload: Value) -> Self {
        Self {
            payload: crate::json::present(payload),
            ..self
        }
    }

    /// Sets [`metadata`](Self::metadata).
    #[must_use]
    pub fn with_metadata(self, metadata: Metadata) -> Self {
        Self {
            metadata: Some(metadata),
            ..self
        }
    }
}
impl Interrupt {
    /// Parses [`expires_at`](Self::expires_at). Hand-written RFC 3339 reader;
    /// no date-time dependency, no allocation.
    #[must_use]
    pub fn expiry(&self) -> Expiry {
        match &self.expires_at {
            None => Expiry::Never,
            Some(text) => match parse_rfc3339(text.as_bytes()) {
                Some(instant) => Expiry::At(instant),
                None => Expiry::Unparseable,
            },
        }
    }

    /// Whether the interrupt is expired at `now`. [`Expiry::Unparseable`] is
    /// never expired.
    #[must_use]
    pub fn is_expired_at(&self, now: SystemTime) -> bool {
        match self.expiry() {
            Expiry::At(instant) => instant <= now,
            Expiry::Never | Expiry::Unparseable => false,
        }
    }
}

impl ResumeEntry {
    /// A resolved entry carrying `payload` (INTR-09).
    pub fn resolved(interrupt_id: impl Into<InterruptId>, payload: Value) -> Self {
        Self::new(interrupt_id, ResumeStatus::Resolved).with_payload(payload)
    }

    /// A cancelled entry: the interrupt is explicitly abandoned (INTR-10, INTR-15).
    pub fn cancelled(interrupt_id: impl Into<InterruptId>) -> Self {
        Self::new(interrupt_id, ResumeStatus::Cancelled)
    }
}

/// Reads an RFC 3339 `date-time`: `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`.
///
/// `T`/`Z` may be lower case and the date and time may be separated by a
/// space (RFC 3339 §5.6 note). A leap second (`:60`) is accepted and lands on
/// the following second. Fractions beyond nanoseconds are truncated. Works on
/// the bytes in place; nothing is allocated.
fn parse_rfc3339(text: &[u8]) -> Option<SystemTime> {
    let year = digits(text, 0, 4)?;
    let month = digits(text, 5, 2)?;
    let day = digits(text, 8, 2)?;
    let hour = digits(text, 11, 2)?;
    let minute = digits(text, 14, 2)?;
    let second = digits(text, 17, 2)?;
    if text.get(4) != Some(&b'-')
        || text.get(7) != Some(&b'-')
        || !matches!(text.get(10), Some(b'T' | b't' | b' '))
        || text.get(13) != Some(&b':')
        || text.get(16) != Some(&b':')
    {
        return None;
    }
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut at = 19;
    let mut nanos: u32 = 0;
    if text.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        let mut scale = 100_000_000;
        while let Some(&byte) = text.get(at) {
            if !byte.is_ascii_digit() {
                break;
            }
            nanos += u32::from(byte - b'0') * scale;
            scale /= 10;
            at += 1;
        }
        if at == start {
            return None;
        }
    }

    let offset_seconds: i64 = match text.get(at)? {
        b'Z' | b'z' if at + 1 == text.len() => 0,
        sign @ (b'+' | b'-') => {
            let hours = digits(text, at + 1, 2)?;
            let minutes = digits(text, at + 4, 2)?;
            if text.get(at + 3) != Some(&b':') || at + 6 != text.len() || hours > 23 || minutes > 59
            {
                return None;
            }
            let magnitude = i64::from(hours * 3600 + minutes * 60);
            if *sign == b'+' {
                magnitude
            } else {
                -magnitude
            }
        }
        _ => return None,
    };

    let days = days_from_civil(i64::from(year), month, day);
    let seconds = days * 86_400 + i64::from(hour * 3600 + minute * 60 + second) - offset_seconds;
    let instant = if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(seconds.unsigned_abs(), nanos))?
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(seconds.unsigned_abs()))?
            .checked_add(Duration::from_nanos(u64::from(nanos)))?
    };
    Some(instant)
}

/// `count` ASCII digits at `at`, as a number.
fn digits(text: &[u8], at: usize, count: usize) -> Option<u32> {
    let slice = text.get(at..at + count)?;
    slice.iter().try_fold(0u32, |acc, &byte| {
        byte.is_ascii_digit()
            .then(|| acc * 10 + u32::from(byte - b'0'))
    })
}

fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given proleptic Gregorian date.
///
/// Counts whole 400-year eras (146 097 days each) from 0000-03-01, so that
/// the leap day falls at the end of each counted year, then shifts the
/// result to the Unix epoch (719 468 days after 0000-03-01).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let (year, month) = if month <= 2 {
        (year - 1, i64::from(month) + 9)
    } else {
        (year, i64::from(month) - 3)
    };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(text: &str) -> Option<i128> {
        parse_rfc3339(text.as_bytes()).map(|t| match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        })
    }

    #[test]
    fn known_instants() {
        assert_eq!(secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            secs("2000-03-01T00:00:00Z"),
            Some(951_868_800 * 1_000_000_000)
        );
        assert_eq!(
            secs("2026-08-20T17:00:00Z"),
            Some(1_787_245_200 * 1_000_000_000)
        );
        assert_eq!(
            secs("2026-08-20T19:00:00+02:00"),
            secs("2026-08-20T17:00:00Z")
        );
        assert_eq!(
            secs("2026-08-20t17:00:00.5z"),
            Some(1_787_245_200_500_000_000)
        );
        assert_eq!(secs("1969-12-31T23:59:59.25Z"), Some(-750_000_000));
        assert!(secs("2024-02-29T00:00:00Z").is_some());
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "2026-08-20",
            "2026-08-20T17:00:00",
            "2026-13-01T00:00:00Z",
            "2025-02-29T00:00:00Z",
            "2026-08-20T24:00:00Z",
            "2026-08-20T17:00:00.Z",
            "2026-08-20T17:00:00+0200",
            "2026-08-20T17:00:00Zjunk",
            "tomorrow",
        ] {
            assert_eq!(secs(bad), None, "{bad}");
        }
    }
}
