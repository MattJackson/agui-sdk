//! Message and tool-result content: text or multimodal parts.
//!
//! Content parts are an open union (PROC-03): an unknown part kind is dropped
//! from its list with a warning and the list survives (PROC-20). Part sources
//! are a closed set; a part whose source kind is unknown is removed whole
//! (PROC-19). File handles are opaque and never fetched (RUNIN-13).
//!
//! Producers that cannot use a part skip it and continue (RUNIN-11, TOOL-17);
//! [`crate::server::helpers::PartFilter`] does that with the required warnings.
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[`crate::server::helpers::PartFilter`]: https://docs.rs/agui-sdk/latest/agui_sdk/server/helpers/struct.PartFilter.html"
)]

use std::borrow::Cow;
use std::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::json::{deserialize_tagged, Tagged};

/// `string | ContentPart[]`, as carried by user messages, tool messages and
/// tool results.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum Content {
    /// Plain text.
    Text(String),
    /// An ordered list of parts.
    Parts(Vec<ContentPart>),
}

impl Content {
    /// The text when this is [`Content::Text`].
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text(text) => Some(text),
            Content::Parts(_) => None,
        }
    }

    /// String-only rendering: `text` parts concatenated in order, everything
    /// else ignored (TOOL-19). Borrows when the content is already a string.
    /// Callers presenting this to a user should say it is lossy (TOOL-20).
    #[must_use]
    pub fn flatten_text(&self) -> Cow<'_, str> {
        let parts = match self {
            Content::Text(text) => return Cow::Borrowed(text),
            Content::Parts(parts) => parts,
        };
        let mut texts = parts.iter().filter_map(|part| match part {
            ContentPart::Text(part) => Some(part.text.as_str()),
            _ => None,
        });
        let Some(first) = texts.next() else {
            return Cow::Borrowed("");
        };
        let Some(second) = texts.next() else {
            return Cow::Borrowed(first);
        };
        let mut joined = String::with_capacity(first.len() + second.len());
        joined.push_str(first);
        joined.push_str(second);
        texts.for_each(|text| joined.push_str(text));
        Cow::Owned(joined)
    }
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        Content::Text(text)
    }
}

impl From<&str> for Content {
    fn from(text: &str) -> Self {
        Content::Text(text.to_owned())
    }
}

impl From<Vec<ContentPart>> for Content {
    fn from(parts: Vec<ContentPart>) -> Self {
        Content::Parts(parts)
    }
}

/// One content part (`$defs/ContentPart`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[non_exhaustive]
pub enum ContentPart {
    /// Text.
    Text(TextPart),
    /// An image.
    Image(MediaPart),
    /// Audio.
    Audio(MediaPart),
    /// Video.
    Video(MediaPart),
    /// A document.
    Document(MediaPart),
}

/// Where a media part's bytes live (`$defs/PartSource`). Closed set (RUNIN-18).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[non_exhaustive]
pub enum PartSource {
    /// Inline base64.
    Data(DataSource),
    /// A URL.
    Url(UrlSource),
    /// A provider file handle.
    File(FileSource),
}

/// A text content part.
///
/// Schema: `$defs/TextPart`. Spec rules: TOOL-15, TOOL-19.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TextPart {
    /// The text.
    pub text: String,
    /// Optional part identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Free-form part annotations; any non-null JSON value. Structured
    /// extras for a tool result ride here (TOOL-15).
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub metadata: Option<Value>,
}

impl TextPart {
    /// Creates the value from its required fields (`text`); every optional
    /// field starts absent.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            id: None,
            metadata: None,
        }
    }

    /// Sets [`id`](Self::id).
    #[must_use]
    pub fn with_id(self, id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            ..self
        }
    }

    /// Sets [`metadata`](Self::metadata).
    #[must_use]
    pub fn with_metadata(self, metadata: Value) -> Self {
        Self {
            metadata: crate::json::present(metadata),
            ..self
        }
    }
}

/// A media content part. The four media kinds share this shape; the
/// [`ContentPart`] variant says which kind it is.
///
/// Schema: `$defs/ImagePart / AudioPart / VideoPart / DocumentPart`. Spec rules: RUNIN-11, RUNIN-13, RUNIN-17.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct MediaPart {
    /// Where the media lives.
    pub source: PartSource,
    /// Optional part identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Free-form part annotations, e.g. a file handle's expiry (RUNIN-17).
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub metadata: Option<Value>,
}

impl MediaPart {
    /// Creates the value from its required fields (`source`); every optional
    /// field starts absent.
    #[must_use]
    pub fn new(source: PartSource) -> Self {
        Self {
            source,
            id: None,
            metadata: None,
        }
    }

    /// Sets [`id`](Self::id).
    #[must_use]
    pub fn with_id(self, id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            ..self
        }
    }

    /// Sets [`metadata`](Self::metadata).
    #[must_use]
    pub fn with_metadata(self, metadata: Value) -> Self {
        Self {
            metadata: crate::json::present(metadata),
            ..self
        }
    }
}

/// Inline base64 media.
///
/// Schema: `$defs/DataSource`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DataSource {
    /// Base64-encoded bytes. Not decoded by the pipeline.
    pub value: String,
    /// Media type of the decoded bytes.
    pub mime_type: String,
}

impl DataSource {
    /// Creates the value from its required fields (`value`, `mime_type`).
    pub fn new(value: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            mime_type: mime_type.into(),
        }
    }
}

/// Media referenced by URL.
///
/// Schema: `$defs/UrlSource`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct UrlSource {
    /// The URL. Never fetched by this SDK.
    pub value: String,
    /// Media type, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl UrlSource {
    /// Creates the value from its required fields (`value`); every optional
    /// field starts absent.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            mime_type: None,
        }
    }

    /// Sets [`mime_type`](Self::mime_type).
    #[must_use]
    pub fn with_mime_type(self, mime_type: impl Into<String>) -> Self {
        Self {
            mime_type: Some(mime_type.into()),
            ..self
        }
    }
}

/// A provider file handle. Opaque: never fetched, parsed, or scheme-sniffed
/// (RUNIN-13, TOOL-16).
///
/// Schema: `$defs/FileSource`. Spec rules: RUNIN-13, RUNIN-14, TOOL-16.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FileSource {
    /// The opaque handle.
    pub value: String,
    /// Lowercase vendor id of the issuing provider (RUNIN-14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Media type, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl FileSource {
    /// Creates the value from its required fields (`value`); every optional
    /// field starts absent.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            provider: None,
            mime_type: None,
        }
    }

    /// Sets [`provider`](Self::provider).
    #[must_use]
    pub fn with_provider(self, provider: impl Into<String>) -> Self {
        Self {
            provider: Some(provider.into()),
            ..self
        }
    }

    /// Sets [`mime_type`](Self::mime_type).
    #[must_use]
    pub fn with_mime_type(self, mime_type: impl Into<String>) -> Self {
        Self {
            mime_type: Some(mime_type.into()),
            ..self
        }
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ContentVisitor)
    }
}

/// Picks the arm from the JSON type itself, so nothing is buffered.
struct ContentVisitor;

impl<'de> Visitor<'de> for ContentVisitor {
    type Value = Content;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string or an array of content parts")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Content, E> {
        Ok(Content::Text(text.to_owned()))
    }

    fn visit_string<E: de::Error>(self, text: String) -> Result<Content, E> {
        Ok(Content::Text(text))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Content, A::Error> {
        let mut parts = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(64));
        while let Some(part) = seq.next_element()? {
            parts.push(part);
        }
        Ok(Content::Parts(parts))
    }
}

/// Wire spellings of [`ContentPart`]'s `type`.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PartKind {
    Text,
    Image,
    Audio,
    Video,
    Document,
}

impl Tagged for ContentPart {
    const TAG: &'static str = "type";
    const EXPECTING: &'static str = "a content part object";
    type Tag = PartKind;

    fn from_tag<'de, D: Deserializer<'de>>(tag: PartKind, rest: D) -> Result<Self, D::Error> {
        Ok(match tag {
            PartKind::Text => ContentPart::Text(TextPart::deserialize(rest)?),
            PartKind::Image => ContentPart::Image(MediaPart::deserialize(rest)?),
            PartKind::Audio => ContentPart::Audio(MediaPart::deserialize(rest)?),
            PartKind::Video => ContentPart::Video(MediaPart::deserialize(rest)?),
            PartKind::Document => ContentPart::Document(MediaPart::deserialize(rest)?),
        })
    }
}

impl<'de> Deserialize<'de> for ContentPart {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_tagged(deserializer)
    }
}

/// Wire spellings of [`PartSource`]'s `type`.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SourceKind {
    Data,
    Url,
    File,
}

impl Tagged for PartSource {
    const TAG: &'static str = "type";
    const EXPECTING: &'static str = "a part source object";
    type Tag = SourceKind;

    fn from_tag<'de, D: Deserializer<'de>>(tag: SourceKind, rest: D) -> Result<Self, D::Error> {
        Ok(match tag {
            SourceKind::Data => PartSource::Data(DataSource::deserialize(rest)?),
            SourceKind::Url => PartSource::Url(UrlSource::deserialize(rest)?),
            SourceKind::File => PartSource::File(FileSource::deserialize(rest)?),
        })
    }
}

impl<'de> Deserialize<'de> for PartSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_tagged(deserializer)
    }
}
