//! Optional `null` read as absent.
//!
//! 1.0 says an optional field with no value is omitted, never `null`
//! (`basic/index.mdx`, "Absent means absent"), and leaves the `null`s the
//! protocol already tolerated to a compatibility shim (BASIC-12). This module
//! is that shim.
//!
//! # How the list is derived
//!
//! A position is on the list when both of these hold:
//!
//! 1. **The schema makes it optional.** The field is a property of its
//!    `$defs` definition in `spec/1.0/schema.json` and is not in that
//!    definition's `required`. A required position is never on the list: a
//!    `null` there is malformed and stays fatal (e.g. `subagentRunId` on the
//!    `SUBAGENT_*` events, `CUSTOM.value` and `STATE_SNAPSHOT.snapshot`, where
//!    `null` is data).
//! 2. **0.x producers sent `null` there.** The evidence is the deprecation
//!    registry (`DEPRECATIONS.md` at the pinned commit), which records each
//!    such position as a retired shape with an expiry. The schema corroborates
//!    it for the unconstrained fields: `rawEvent`, `result`, `payload`,
//!    `parameters`, `forwardedProps` and the media parts' `metadata` are
//!    exactly the optional "any JSON value" fields the schema annotates with
//!    `not: {type: null}`, which it does because a `null` there was common
//!    enough to need excluding by name. The typed fields on the list
//!    (`parentMessageId`, `outcome`, the usage counts, ...) were declared
//!    nullable by the 0.x SDK types. `RunAgentInput.state` is on the list
//!    because the registry says its existing `state: null` tolerance
//!    continues (read as absent), though it has no row of its own.
//!
//! The rules are keyed by schema definition rather than by position, so a
//! definition that appears in several places (a content part in a snapshot
//! message, a usage entry on `RUN_ERROR`) is treated the same everywhere.
//! Positions that fail either test are deliberately absent: event or message
//! `metadata: null` (never tolerated), the text part's `metadata` (new in 1.0,
//! so no older producer sent it), and any `null` under an open-by-key object,
//! which is data and preserved. A unit test checks every rule against the
//! schema when the spec cache is present.

use serde_json::{Map, Value};

use super::{Kind, Notes, Notice, Path};
use crate::diagnostics::DiagnosticCode;
use crate::pipeline::shapes::DefId;

/// One position of the optional-null list. The discriminant is the rule's
/// notice bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Rule {
    RawEvent,
    SubagentRunId,
    ToolCallStartParent,
    ToolCallChunkParent,
    RunStartedParentRunId,
    RunStartedInput,
    RunFinishedOutcome,
    RunFinishedResult,
    RunErrorUsage,
    SubagentFinishedResult,
    ToolCallResultRole,
    PendingToolCallIds,
    UsageProvider,
    UsageReasoningTokens,
    UsageCachedInputTokens,
    UsageCacheWriteInputTokens,
    ResumePayload,
    PartMetadata,
    ToolParameters,
    ForwardedProps,
    RunInputState,
}

impl Rule {
    pub(crate) const ALL: &'static [Rule] = &[
        Rule::RawEvent,
        Rule::SubagentRunId,
        Rule::ToolCallStartParent,
        Rule::ToolCallChunkParent,
        Rule::RunStartedParentRunId,
        Rule::RunStartedInput,
        Rule::RunFinishedOutcome,
        Rule::RunFinishedResult,
        Rule::RunErrorUsage,
        Rule::SubagentFinishedResult,
        Rule::ToolCallResultRole,
        Rule::PendingToolCallIds,
        Rule::UsageProvider,
        Rule::UsageReasoningTokens,
        Rule::UsageCachedInputTokens,
        Rule::UsageCacheWriteInputTokens,
        Rule::ResumePayload,
        Rule::PartMetadata,
        Rule::ToolParameters,
        Rule::ForwardedProps,
        Rule::RunInputState,
    ];

    pub(crate) const COUNT: u32 = Rule::ALL.len() as u32;

    /// The field name.
    pub(crate) fn field(self) -> &'static str {
        match self {
            Rule::RawEvent => "rawEvent",
            Rule::SubagentRunId => "subagentRunId",
            Rule::ToolCallStartParent | Rule::ToolCallChunkParent => "parentMessageId",
            Rule::RunStartedParentRunId => "parentRunId",
            Rule::RunStartedInput => "input",
            Rule::RunFinishedOutcome => "outcome",
            Rule::RunFinishedResult | Rule::SubagentFinishedResult => "result",
            Rule::RunErrorUsage => "usage",
            Rule::ToolCallResultRole => "role",
            Rule::PendingToolCallIds => "pendingToolCallIds",
            Rule::UsageProvider => "provider",
            Rule::UsageReasoningTokens => "reasoningTokens",
            Rule::UsageCachedInputTokens => "cachedInputTokens",
            Rule::UsageCacheWriteInputTokens => "cacheWriteInputTokens",
            Rule::ResumePayload => "payload",
            Rule::PartMetadata => "metadata",
            Rule::ToolParameters => "parameters",
            Rule::ForwardedProps => "forwardedProps",
            Rule::RunInputState => "state",
        }
    }

    /// The schema definitions the field is optional in.
    pub(crate) fn definitions(self) -> &'static [&'static str] {
        match self {
            Rule::RawEvent => &["BaseEvent"],
            Rule::SubagentRunId => &["Attributable"],
            Rule::ToolCallStartParent => &["ToolCallStartEvent"],
            Rule::ToolCallChunkParent => &["ToolCallChunkEvent"],
            Rule::RunStartedParentRunId | Rule::RunStartedInput => &["RunStartedEvent"],
            Rule::RunFinishedOutcome | Rule::RunFinishedResult => &["RunFinishedEvent"],
            Rule::RunErrorUsage => &["RunErrorEvent"],
            Rule::SubagentFinishedResult => &["SubagentFinishedEvent"],
            Rule::ToolCallResultRole => &["ToolCallResultEvent"],
            Rule::PendingToolCallIds => &["RunFinishedSuccessOutcome"],
            Rule::UsageProvider
            | Rule::UsageReasoningTokens
            | Rule::UsageCachedInputTokens
            | Rule::UsageCacheWriteInputTokens => &["TokenUsage"],
            Rule::ResumePayload => &["ResumeEntry"],
            Rule::PartMetadata => &["ImagePart", "AudioPart", "VideoPart", "DocumentPart"],
            Rule::ToolParameters => &["Tool"],
            Rule::ForwardedProps | Rule::RunInputState => &["RunAgentInput"],
        }
    }

    /// Where the field lives, for the warning.
    fn owner(self) -> &'static str {
        match self {
            Rule::RawEvent | Rule::SubagentRunId => "an event",
            Rule::ToolCallStartParent => "TOOL_CALL_START",
            Rule::ToolCallChunkParent => "TOOL_CALL_CHUNK",
            Rule::RunStartedParentRunId | Rule::RunStartedInput => "RUN_STARTED",
            Rule::RunFinishedOutcome | Rule::RunFinishedResult => "RUN_FINISHED",
            Rule::RunErrorUsage => "RUN_ERROR",
            Rule::SubagentFinishedResult => "SUBAGENT_FINISHED",
            Rule::ToolCallResultRole => "TOOL_CALL_RESULT",
            Rule::PendingToolCallIds => "a success outcome",
            Rule::UsageProvider
            | Rule::UsageReasoningTokens
            | Rule::UsageCachedInputTokens
            | Rule::UsageCacheWriteInputTokens => "a usage entry",
            Rule::ResumePayload => "a resume entry",
            Rule::PartMetadata => "a media content part",
            Rule::ToolParameters => "a tool",
            Rule::ForwardedProps | Rule::RunInputState => "the run input",
        }
    }
}

/// Whether the list reads a whole `null` for `field` of an object of the
/// definition `anchor`, composing `fragments`, as absent. Enforcement asks
/// this, so a host running the enforcer on its own and the server's input
/// admission tolerate exactly the list and nothing more: any other `null` in
/// an optional position is the type mismatch the schema says it is.
///
/// A rule keyed by a fragment (`BaseEvent`, `Attributable`) applies to every
/// definition composing it, except that `subagentRunId: null` is listed on
/// events only, as the boundary reads it.
pub(crate) fn listed(anchor: &str, fragments: &[DefId], field: &str) -> bool {
    let composes = |name: &str| fragments.iter().any(|f| f.name() == name);
    Rule::ALL.iter().any(|rule| {
        rule.field() == field
            && match rule {
                Rule::RawEvent => composes("BaseEvent"),
                Rule::SubagentRunId => composes("Attributable") && composes("BaseEvent"),
                _ => rule.definitions().contains(&anchor),
            }
    })
}

/// Whether the event-level rules would remove anything (no allocation).
pub(super) fn has_event_null(map: &Map<String, Value>, kind: Kind) -> bool {
    is_null(map, Rule::RawEvent.field())
        || (!kind.is_subagent_lifecycle() && is_null(map, Rule::SubagentRunId.field()))
}

/// Applies the event-level rules for one received event; the nested run
/// input's rules run separately ([`input`], from the boundary's
/// `input_content`). The content parts inside messages are handled by the
/// media walk.
pub(super) fn event(map: &mut Map<String, Value>, kind: Kind, notes: &mut Notes<'_, '_>) {
    let root = Path::ROOT;
    take(map, Rule::RawEvent, &root, notes);
    // Required on the subagent lifecycle events: a null there stays malformed.
    if !kind.is_subagent_lifecycle() {
        take(map, Rule::SubagentRunId, &root, notes);
    }
    match kind {
        Kind::ToolCallStart => take(map, Rule::ToolCallStartParent, &root, notes),
        Kind::ToolCallChunk => take(map, Rule::ToolCallChunkParent, &root, notes),
        Kind::ToolCallResult => take(map, Rule::ToolCallResultRole, &root, notes),
        Kind::SubagentFinished => take(map, Rule::SubagentFinishedResult, &root, notes),
        Kind::RunStarted => {
            take(map, Rule::RunStartedParentRunId, &root, notes);
            take(map, Rule::RunStartedInput, &root, notes);
        }
        Kind::RunFinished => {
            take(map, Rule::RunFinishedOutcome, &root, notes);
            take(map, Rule::RunFinishedResult, &root, notes);
            if let Some(Value::Object(outcome)) = map.get_mut("outcome") {
                if outcome.get("type").and_then(Value::as_str) == Some("success") {
                    take(
                        outcome,
                        Rule::PendingToolCallIds,
                        &root.key("outcome"),
                        notes,
                    );
                }
            }
            usage(map, &root, notes);
        }
        Kind::RunError => {
            take(map, Rule::RunErrorUsage, &root, notes);
            usage(map, &root, notes);
        }
        _ => {}
    }
}

/// Applies the rules for a run input object (sent, received or echoed).
pub(super) fn input(map: &mut Map<String, Value>, path: &Path<'_>, notes: &mut Notes<'_, '_>) {
    take(map, Rule::ForwardedProps, path, notes);
    take(map, Rule::RunInputState, path, notes);
    each_object(map, "tools", path, notes, |tool, at, notes| {
        take(tool, Rule::ToolParameters, at, notes)
    });
    each_object(map, "resume", path, notes, |entry, at, notes| {
        take(entry, Rule::ResumePayload, at, notes)
    });
}

/// Applies the rules for one content part.
pub(super) fn part(map: &mut Map<String, Value>, path: &Path<'_>, notes: &mut Notes<'_, '_>) {
    if matches!(
        map.get("type").and_then(Value::as_str),
        Some("image" | "audio" | "video" | "document")
    ) {
        take(map, Rule::PartMetadata, path, notes);
    }
}

fn usage(map: &mut Map<String, Value>, path: &Path<'_>, notes: &mut Notes<'_, '_>) {
    each_object(map, "usage", path, notes, |entry, at, notes| {
        for rule in [
            Rule::UsageProvider,
            Rule::UsageReasoningTokens,
            Rule::UsageCachedInputTokens,
            Rule::UsageCacheWriteInputTokens,
        ] {
            take(entry, rule, at, notes);
        }
    });
}

/// Calls `apply` on every object element of the array at `map[key]`.
fn each_object(
    map: &mut Map<String, Value>,
    key: &'static str,
    path: &Path<'_>,
    notes: &mut Notes<'_, '_>,
    mut apply: impl FnMut(&mut Map<String, Value>, &Path<'_>, &mut Notes<'_, '_>),
) {
    let Some(Value::Array(items)) = map.get_mut(key) else {
        return;
    };
    let list = path.key(key);
    for (index, item) in items.iter_mut().enumerate() {
        if let Value::Object(object) = item {
            apply(object, &list.index(index), notes);
        }
    }
}

fn is_null(map: &Map<String, Value>, key: &str) -> bool {
    matches!(map.get(key), Some(Value::Null))
}

/// Removes `rule`'s field from `map` when it is exactly `null`, noting it once
/// per run.
fn take(map: &mut Map<String, Value>, rule: Rule, path: &Path<'_>, notes: &mut Notes<'_, '_>) {
    let field = rule.field();
    if !is_null(map, field) {
        return;
    }
    super::remove_key(map, field);
    notes.notice(
        Notice::Null(rule),
        DiagnosticCode::OptionalNull,
        &path.key(field),
        || {
            format!(
                "`{field}: null` on {} read as absent; an optional field with no value \
                 must be omitted, so update the producer (shim expires {})",
                rule.owner(),
                super::SHIM_EXPIRY
            )
        },
    );
}

#[cfg(test)]
mod tests {
    use super::Rule;

    /// The schema of the pinned spec; `None` only when the cache is absent
    /// and not required. A cache whose schema cannot be read or parsed fails.
    fn schema() -> Option<serde_json::Value> {
        let path = crate::test_spec::root()?.join("spec/1.0/schema.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        Some(
            serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display())),
        )
    }

    #[test]
    fn every_rule_names_an_optional_schema_field() {
        let Some(schema) = schema() else {
            return;
        };
        for rule in Rule::ALL {
            for definition in rule.definitions() {
                let def = &schema["$defs"][*definition];
                assert!(
                    def["properties"].get(rule.field()).is_some(),
                    "{rule:?}: {definition} has no {}",
                    rule.field()
                );
                let required = def["required"].as_array().cloned().unwrap_or_default();
                assert!(
                    !required.iter().any(|name| name == rule.field()),
                    "{rule:?}: {} is required on {definition}",
                    rule.field()
                );
            }
        }
    }

    #[test]
    fn rules_are_listed_once_in_discriminant_order() {
        for (index, rule) in Rule::ALL.iter().enumerate() {
            assert_eq!(*rule as usize, index);
        }
    }
}
