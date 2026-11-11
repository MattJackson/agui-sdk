//! `metadata`: open annotations on events, messages, tool calls, tools,
//! interrupts and resume entries (META-17).
//!
//! Metadata is a [`JsonObject`] with two protocol rules on top:
//!
//! * **Merging.** Metadata on the events that build one item (a text message,
//!   a tool call, a reasoning message, an activity message) merges into that
//!   item key by key, last write wins, never recursively (META-06..META-13).
//!   The reducer does this with [`JsonObject::merge_from`]; events without a
//!   merge target (run lifecycle, steps, snapshots, RAW, CUSTOM) merge nowhere
//!   (META-14). Stored metadata is plain data afterwards (META-16).
//! * **Keys.** `ag-ui` is reserved for the protocol (META-04) and invented
//!   keys should carry a vendor prefix (META-05). [`MetadataBuilder`] applies
//!   the prefix and refuses the reserved key: silently through `insert`, with
//!   a `metadata.reserved_key` diagnostic through `try_insert`.
//!
//! Unknown keys are never stripped and never warned about (META-03, PROC-37).

pub use crate::json::JsonObject;

use crate::diagnostics::{Diagnostic, DiagnosticCode, Scope};

/// Metadata is an open JSON object; see the module docs for its merge rules.
pub type Metadata = JsonObject;

/// The key reserved for the protocol itself (META-04).
pub const RESERVED_KEY: &str = "ag-ui";

/// Separator between a vendor prefix and a key: `acme.trace_id`.
pub const VENDOR_SEPARATOR: char = '.';

/// Joins a vendor prefix and a key: `vendor_key("acme", "trace_id")` is
/// `"acme.trace_id"` (META-05, PASS-07).
#[must_use]
pub fn vendor_key(prefix: &str, key: &str) -> String {
    let mut joined = String::with_capacity(prefix.len() + 1 + key.len());
    joined.push_str(prefix);
    joined.push(VENDOR_SEPARATOR);
    joined.push_str(key);
    joined
}

/// Builds producer-side metadata with a fixed vendor prefix (META-05).
///
/// ```
/// use agui_sdk::metadata::MetadataBuilder;
/// let meta = MetadataBuilder::vendor("acme").insert("trace_id", "t-1").build();
/// assert!(meta.contains_key("acme.trace_id"));
/// ```
#[derive(Debug, Clone, Default)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct MetadataBuilder {
    prefix: Option<String>,
    object: JsonObject,
}

impl MetadataBuilder {
    /// A builder that writes keys exactly as given.
    pub fn new() -> Self {
        Self::default()
    }

    /// A builder that prefixes every key with `prefix.`.
    pub fn vendor(prefix: impl Into<String>) -> Self {
        Self {
            prefix: Some(prefix.into()),
            object: JsonObject::new(),
        }
    }

    /// Adds a member. Writing [`RESERVED_KEY`] is refused: the member is
    /// skipped, with no side effect. Use [`try_insert`](Self::try_insert) to
    /// learn about the refusal.
    pub fn insert(mut self, key: impl AsRef<str>, value: impl Into<serde_json::Value>) -> Self {
        let key = self.full_key(key.as_ref());
        if key != RESERVED_KEY {
            self.object.insert(key, value);
        }
        self
    }

    /// Adds a member, or returns the `metadata.reserved_key` diagnostic for
    /// [`RESERVED_KEY`] (META-04) for the caller to report.
    ///
    /// # Errors
    ///
    /// The refusal, as a diagnostic.
    pub fn try_insert(
        mut self,
        key: impl AsRef<str>,
        value: impl Into<serde_json::Value>,
    ) -> Result<Self, crate::diagnostics::Diagnostic> {
        let key = self.full_key(key.as_ref());
        if key == RESERVED_KEY {
            return Err(Diagnostic::new(
                DiagnosticCode::ReservedMetadataKey,
                Scope::Stream,
                "/ag-ui",
                "metadata builder skipped the protocol-reserved key `ag-ui`",
            ));
        }
        self.object.insert(key, value);
        Ok(self)
    }

    /// The key as written: prefixed for a vendor builder.
    fn full_key(&self, key: &str) -> String {
        match &self.prefix {
            Some(prefix) => vendor_key(prefix, key),
            None => key.to_owned(),
        }
    }

    /// Finishes the object.
    #[must_use]
    pub fn build(self) -> Metadata {
        self.object
    }
}

/// Lowercase vendor identifiers for [`TokenUsage::provider`](crate::usage::TokenUsage::provider)
/// and [`FileSource::provider`](crate::content::FileSource::provider) (RUNIN-14).
pub mod provider {
    /// Anthropic.
    pub const ANTHROPIC: &str = "anthropic";
    /// OpenAI.
    pub const OPENAI: &str = "openai";
    /// Google.
    pub const GOOGLE: &str = "google";
    /// Microsoft Azure.
    pub const AZURE: &str = "azure";
    /// AWS Bedrock.
    pub const BEDROCK: &str = "bedrock";
    /// Mistral.
    pub const MISTRAL: &str = "mistral";

    /// Normalises a provider name to the lowercase vendor-id form the spec
    /// asks for: trimmed, lowercased. Allocates only when the input changes.
    #[must_use]
    pub fn normalize(name: &str) -> std::borrow::Cow<'_, str> {
        let trimmed = name.trim();
        if trimmed
            .chars()
            .flat_map(char::to_lowercase)
            .eq(trimmed.chars())
        {
            std::borrow::Cow::Borrowed(trimmed)
        } else {
            std::borrow::Cow::Owned(trimmed.to_lowercase())
        }
    }
}
