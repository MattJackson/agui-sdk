//! The era shims behind [`EraShims`](super::EraShims).
//!
//! Every pinned era predates 0.0.58, so every era gets the subagent shim: the
//! `SUBAGENT_*` lifecycle events are dropped with a warning (a removed event
//! is lossy, `versioning.mdx` "Lossy"), and `subagentRunId` is stripped from
//! the survivors without one (the spec's own example of a lossless
//! downgrade). Only a well-formed (string) `subagentRunId` is stripped; any
//! other value is left for enforcement to reject.
//!
//! In the producer direction, a reader at 0.0.47 or below predates media
//! parts and gets legacy `binary` parts instead (see `media::downgrade_parts`). The typed producer path
//! (`Direction::OutboundTyped`, used by `Runner` with `RunnerConfig::legacy_peer`)
//! drops media parts with a warning instead (see `media::drop_parts`).
//!
//! A peer at or below 0.0.39 reads tool results and user content as plain
//! strings, so content parts are flattened: the text parts are concatenated
//! in order, as `Content::flatten_text` does, and every other part is dropped
//! with its own warning. No placeholder text stands in for a dropped part
//! (that would be inventing content, VER-04). Content that holds a malformed
//! part (not an object, no string `type`, a text part without string `text`)
//! is left whole, so the defect still reaches enforcement (VER-05).

use serde_json::{Map, Value};

use super::{Kind, Notes, Path};
use crate::diagnostics::{Diagnostic, DiagnosticCode, Reporter, Scope};
use crate::version::LegacyEra;

/// The era's version, for warnings.
fn label(era: LegacyEra) -> &'static str {
    match era {
        LegacyEra::V0_0_39 => "0.0.39",
        LegacyEra::V0_0_47 => "0.0.47",
        LegacyEra::V0_0_57 => "0.0.57",
    }
}

/// Whether the era predates content parts.
fn strings_only(era: LegacyEra) -> bool {
    era <= LegacyEra::V0_0_39
}

/// Whether the era predates media parts (reads legacy `binary` parts).
fn legacy_binary(era: LegacyEra) -> bool {
    era <= LegacyEra::V0_0_47
}

/// Whether the era predates subagents.
fn no_subagents(era: LegacyEra) -> bool {
    era <= LegacyEra::V0_0_57
}

/// Which way a frame travels through the event shim.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// From the peer into this crate.
    Inbound,
    /// Producer direction, on the JSON form: additionally flattens content
    /// for a strings-only reader and rewrites media parts to legacy `binary`.
    Outbound,
    /// Producer direction for a caller that reads the result back into an
    /// `Event`, which cannot hold the legacy `binary` shape: media parts a
    /// pre-0.0.48 reader cannot take are dropped with a warning instead.
    OutboundTyped,
}

/// The shared event shim.
pub(super) fn event(
    era: LegacyEra,
    frame: &mut Value,
    reporter: &mut Reporter,
    direction: Direction,
) -> bool {
    let outgoing = direction != Direction::Inbound;
    let downgrade_content: ContentShim = if direction == Direction::OutboundTyped {
        downgrade_content_typed
    } else {
        downgrade_content
    };
    let Value::Object(map) = frame else {
        return true;
    };
    let kind = match map.get("type") {
        Some(Value::String(name)) => Kind::of(name),
        _ => return true,
    };
    if no_subagents(era) {
        if kind.is_subagent_lifecycle() {
            let name = kind.wire_name().unwrap_or_default();
            let event_type = kind.event_type();
            reporter.report(|| {
                let diagnostic = Diagnostic::new(
                    DiagnosticCode::LossyDowngrade,
                    Scope::Event,
                    "",
                    format!(
                        "peer pinned to {} predates subagents: dropping {name}; its \
                         subagent's work still arrives, unattributed; upgrade the peer to \
                         0.0.58 or later",
                        label(era)
                    ),
                );
                match event_type {
                    Some(event_type) => diagnostic.with_event_type(event_type),
                    None => diagnostic,
                }
            });
            return false;
        }
        strip_attribution(map, kind);
    }
    if outgoing && legacy_binary(era) {
        let mut noticed = 0;
        let mut notes = Notes {
            reporter,
            scope: Scope::Event,
            event_type: kind.event_type(),
            noticed: &mut noticed,
            inbound: false,
        };
        let root = Path::ROOT;
        match kind {
            Kind::ToolCallResult => {
                if let Some(content) = map.get_mut("content") {
                    downgrade_content(era, content, &root.key("content"), &mut notes);
                }
            }
            Kind::MessagesSnapshot => {
                each_content(era, map, &root, &mut notes, downgrade_content);
            }
            Kind::RunStarted => {
                if let Some(Value::Object(input)) = map.get_mut("input") {
                    let at = root.key("input");
                    each_content(era, input, &at, &mut notes, downgrade_content);
                }
            }
            _ => {}
        }
    }
    true
}

type ContentShim = fn(LegacyEra, &mut Value, &Path<'_>, &mut Notes<'_, '_>);

/// [`downgrade_content`] for a result that must stay a typed `Content`: a
/// media part a 0.0.47 reader cannot take is dropped, warning for each.
fn downgrade_content_typed(
    era: LegacyEra,
    content: &mut Value,
    path: &Path<'_>,
    notes: &mut Notes<'_, '_>,
) {
    if strings_only(era) {
        flatten(era, content, path, notes);
    } else if let Value::Array(parts) = content {
        super::media::drop_parts(parts, path, notes, label(era));
        if parts.is_empty() {
            *content = Value::String(String::new());
        }
    }
}

/// Producer direction: the content shape a reader of `era` takes.
fn downgrade_content(
    era: LegacyEra,
    content: &mut Value,
    path: &Path<'_>,
    notes: &mut Notes<'_, '_>,
) {
    if strings_only(era) {
        flatten(era, content, path, notes);
    } else if let Value::Array(parts) = content {
        super::media::downgrade_parts(parts, path, notes, label(era));
    }
}

/// The input shim.
pub(super) fn input(era: LegacyEra, input: &mut Value, reporter: &mut Reporter) {
    let Value::Object(map) = input else {
        return;
    };
    // Every era predates the field (RUNIN-08, VER-17).
    super::remove_key(map, "protocolVersion");
    if no_subagents(era) {
        strip_messages(map);
    }
    if strings_only(era) {
        let mut noticed = 0;
        let mut notes = Notes {
            reporter,
            scope: Scope::RunInput,
            event_type: None,
            noticed: &mut noticed,
            inbound: false,
        };
        each_content(era, map, &Path::ROOT, &mut notes, flatten);
    }
}

/// Removes a string `subagentRunId` from `map`.
fn strip(map: &mut Map<String, Value>) {
    if matches!(map.get("subagentRunId"), Some(Value::String(_))) {
        super::remove_key(map, "subagentRunId");
    }
}

/// Strips attribution from each message of `map["messages"]`.
fn strip_messages(map: &mut Map<String, Value>) {
    if let Some(Value::Array(messages)) = map.get_mut("messages") {
        for message in messages {
            if let Value::Object(message) = message {
                strip(message);
            }
        }
    }
}

/// Strips attribution from an event and from what it carries.
fn strip_attribution(map: &mut Map<String, Value>, kind: Kind) {
    strip(map);
    match kind {
        Kind::MessagesSnapshot => strip_messages(map),
        Kind::RunStarted => {
            if let Some(Value::Object(input)) = map.get_mut("input") {
                strip_messages(input);
            }
        }
        Kind::RunFinished => {
            if let Some(Value::Array(interrupts)) = map
                .get_mut("outcome")
                .and_then(|outcome| outcome.get_mut("interrupts"))
            {
                for interrupt in interrupts {
                    if let Value::Object(interrupt) = interrupt {
                        strip(interrupt);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Applies `apply` to the content of each message of `map["messages"]`.
fn each_content(
    era: LegacyEra,
    map: &mut Map<String, Value>,
    path: &Path<'_>,
    notes: &mut Notes<'_, '_>,
    apply: fn(LegacyEra, &mut Value, &Path<'_>, &mut Notes<'_, '_>),
) {
    let Some(Value::Array(messages)) = map.get_mut("messages") else {
        return;
    };
    let list = path.key("messages");
    for (index, message) in messages.iter_mut().enumerate() {
        if let Some(content) = message.get_mut("content") {
            let at = list.index(index);
            apply(era, content, &at.key("content"), notes);
        }
    }
}

/// The part types a warning may name (anything else is not repeated back).
fn part_label(kind: &str) -> &'static str {
    match kind {
        "image" => "an image",
        "audio" => "an audio",
        "video" => "a video",
        "document" => "a document",
        "binary" => "a legacy binary",
        _ => "an unrecognised",
    }
}

/// Whether every part is shaped well enough to flatten without hiding a
/// defect.
fn well_formed(parts: &[Value]) -> bool {
    parts.iter().all(|part| match part.get("type") {
        Some(Value::String(kind)) if kind == "text" => {
            matches!(part.get("text"), Some(Value::String(_)))
        }
        Some(Value::String(_)) => true,
        _ => false,
    })
}

/// Replaces a list of parts with the concatenation of its text parts,
/// warning for each part dropped (TOOL-22). Leaves a string, and a list with
/// a malformed part, untouched.
fn flatten(era: LegacyEra, content: &mut Value, path: &Path<'_>, notes: &mut Notes<'_, '_>) {
    let Value::Array(parts) = content else {
        return;
    };
    if !well_formed(parts) {
        return;
    }
    let mut text = String::new();
    for (index, part) in parts.iter().enumerate() {
        let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
        let at = path.index(index);
        if kind == "text" {
            text.push_str(part.get("text").and_then(Value::as_str).unwrap_or_default());
            if part.get("metadata").is_some() {
                notes.lossy(DiagnosticCode::LossyDowngrade, &at.key("metadata"), || {
                    format!(
                        "peer pinned to {} takes string content only: a text part's metadata \
                         was dropped; upgrade the peer to 0.0.40 or later",
                        label(era)
                    )
                });
            }
        } else {
            notes.lossy(DiagnosticCode::LossyDowngrade, &at, || {
                format!(
                    "peer pinned to {} takes string content only and DROPS non-text parts: \
                     {} part was dropped; upgrade the peer to 0.0.40 or later",
                    label(era),
                    part_label(kind)
                )
            });
        }
    }
    *content = Value::String(text);
}
