//! Legacy binary content parts upgraded to 1.0 media parts.
//!
//! The 0.x part `{"type": "binary", "mimeType", "data"?, "url"?, "id"?,
//! "filename"?}` named its bytes in one of three ways: inline base64 (`data`),
//! a URL (`url`), or a handle already at the provider (`id`). 1.0 splits the
//! part by what it is (`image`, `audio`, `video`, `document`) and moves the
//! bytes into a `source` (`data`, `url`, `file`), so the upgrade is:
//!
//! * the part type comes from the MIME type's top-level type: `image/*`,
//!   `audio/*` and `video/*` map to their parts, everything else is a
//!   `document`;
//! * the source is `data` when the part carried inline bytes, else `url`,
//!   else `file`, and carries the MIME type. A part that named its bytes in
//!   more than one way keeps the first and warns for each other (lossy);
//! * `filename` has no field of its own in 1.0 and moves to the part's
//!   `metadata` (which is unconstrained on media parts);
//! * any other member is carried over for enforcement to judge.
//!
//! A legacy part without a string `mimeType`, or with no string source, or
//! with a source or filename that is not a string, is malformed and left
//! exactly as it is: enforcement then strips it as an unrecognised part, with
//! its own warning. The upgrade never repairs.
//!
//! Producers that sent both shapes during the transition (a modern part plus
//! its legacy mirror) would otherwise duplicate the attachment, so an upgraded
//! part identical in type and source to a modern part of the same list, whose
//! filename that modern part already keeps, and which carries no other member
//! (nor other metadata), is removed. Nothing is lost.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hash, Hasher};

use serde_json::{Map, Value};

use super::{absent, Notes, Notice, Path};
use crate::diagnostics::DiagnosticCode;

/// Upgrades one list of content parts: optional nulls first, then legacy
/// binary parts. Allocation-free when the list has no legacy part.
pub(super) fn parts(parts: &mut Vec<Value>, path: &Path<'_>, notes: &mut Notes<'_, '_>) {
    let mut legacy = false;
    for (index, part) in parts.iter_mut().enumerate() {
        if let Value::Object(map) = part {
            absent::part(map, &path.index(index), notes);
            legacy |= is_legacy(map);
        }
    }
    if legacy {
        upgrade_all(parts, path, notes);
    }
}

fn is_legacy(map: &Map<String, Value>) -> bool {
    map.get("type").and_then(Value::as_str) == Some("binary")
}

fn upgrade_all(parts: &mut Vec<Value>, path: &Path<'_>, notes: &mut Notes<'_, '_>) {
    let modern: Vec<bool> = parts
        .iter()
        .map(|part| part.as_object().is_some_and(|map| !is_legacy(map)))
        .collect();
    let mut upgraded = Vec::new();
    for (index, part) in parts.iter_mut().enumerate() {
        let Value::Object(map) = part else {
            continue;
        };
        if is_legacy(map) && upgrade(map, &path.index(index), notes) {
            upgraded.push(index);
        }
    }
    if upgraded.is_empty() {
        return;
    }
    let mirror = mirrored(parts, &modern, &upgraded);
    // Retaining keeps the order of the rest.
    let mut index = 0;
    parts.retain(|_| {
        let keep = !mirror[index];
        index += 1;
        keep
    });
}

/// The filenames a bucket of modern parts carries: strings hashed directly,
/// anything else (out of shape) bucketed by an order-independent structural
/// hash and confirmed with a `Value` comparison, which is what the matching
/// always was (object equality ignores key order).
#[derive(Default)]
struct Names<'a> {
    strings: HashSet<&'a str>,
    others: HashMap<u64, Vec<&'a Value>>,
}

/// A structural hash under which equal values (object key order ignored) hash
/// equally. Linear in the size of the value.
fn shape_hash(value: &Value, state: &RandomState) -> u64 {
    let mut hasher = state.build_hasher();
    match value {
        Value::Null => 0u8.hash(&mut hasher),
        Value::Bool(b) => (1u8, *b).hash(&mut hasher),
        // Mirrors serde_json's number equality: integers by variant, floats
        // by value (so `-0.0 == 0.0` hash alike).
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                (2u8, u).hash(&mut hasher);
            } else if let Some(i) = n.as_i64() {
                (6u8, i).hash(&mut hasher);
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                (7u8, if f == 0.0 { 0.0f64 } else { f }.to_bits()).hash(&mut hasher);
            }
        }
        Value::String(s) => (3u8, s).hash(&mut hasher),
        Value::Array(items) => {
            4u8.hash(&mut hasher);
            for item in items {
                shape_hash(item, state).hash(&mut hasher);
            }
        }
        Value::Object(map) => {
            5u8.hash(&mut hasher);
            // Commutative over members, so key order does not matter.
            let mut sum = 0u64;
            for (key, member) in map {
                let mut inner = state.build_hasher();
                key.hash(&mut inner);
                shape_hash(member, state).hash(&mut inner);
                sum = sum.wrapping_add(inner.finish());
            }
            sum.hash(&mut hasher);
        }
    }
    hasher.finish()
}

impl<'a> Names<'a> {
    fn insert(&mut self, filename: &'a Value, state: &RandomState) {
        match filename {
            Value::String(s) => {
                self.strings.insert(s);
            }
            other => self
                .others
                .entry(shape_hash(other, state))
                .or_default()
                .push(other),
        }
    }

    fn contains(&self, filename: &Value, state: &RandomState) -> bool {
        match filename {
            Value::String(s) => self.strings.contains(s.as_str()),
            other => self
                .others
                .get(&shape_hash(other, state))
                .is_some_and(|bucket| bucket.contains(&other)),
        }
    }
}

/// The key of a part: its type and the four members an upgraded source has.
/// `None` for a part that cannot equal an upgraded one.
fn mirror_key(part: &Value) -> Option<(&str, &str, &str, &str)> {
    let kind = part.get("type")?.as_str()?;
    let source = part.get("source")?.as_object()?;
    if source.len() != 3 {
        return None;
    }
    Some((
        kind,
        source.get("type")?.as_str()?,
        source.get("value")?.as_str()?,
        source.get("mimeType")?.as_str()?,
    ))
}

/// For each part, whether it is a converted legacy part (`upgraded` lists
/// their indices) that only repeats a modern part: the same type and source,
/// and, when it names a file, the same filename. Linear in the list.
fn mirrored(parts: &[Value], modern: &[bool], upgraded: &[usize]) -> Vec<bool> {
    let state = RandomState::new();
    let mut index: HashMap<(&str, &str, &str, &str), Names<'_>> = HashMap::new();
    for (part, _) in parts
        .iter()
        .zip(modern)
        .filter(|(_, is_modern)| **is_modern)
    {
        if let Some(key) = mirror_key(part) {
            let names = index.entry(key).or_default();
            if let Some(filename) = part.pointer("/metadata/filename") {
                names.insert(filename, &state);
            }
        }
    }
    let mut mirror = vec![false; parts.len()];
    for &at in upgraded {
        let part = &parts[at];
        mirror[at] = only_key_members(part)
            && mirror_key(part)
                .and_then(|key| index.get(&key))
                .is_some_and(|names| match part.pointer("/metadata/filename") {
                    None => true,
                    Some(filename) => names.contains(filename, &state),
                });
    }
    mirror
}

/// Whether an upgraded part carries nothing beyond what [`mirror_key`] and
/// the filename compare: `type`, `source`, and `metadata` holding at most a
/// `filename`. Anything else would be lost with the part.
fn only_key_members(part: &Value) -> bool {
    let Some(map) = part.as_object() else {
        return false;
    };
    map.iter().all(|(key, value)| match key.as_str() {
        "type" | "source" => true,
        "metadata" => value
            .as_object()
            .is_some_and(|metadata| metadata.keys().all(|k| k == "filename")),
        _ => false,
    })
}

/// The source members of a legacy part, in order of preference.
const SOURCES: [(&str, &str); 3] = [("data", "data"), ("url", "url"), ("id", "file")];

/// Converts one legacy part in place. Returns `false`, leaving it untouched,
/// when it is malformed.
fn upgrade(map: &mut Map<String, Value>, path: &Path<'_>, notes: &mut Notes<'_, '_>) -> bool {
    let Some(Value::String(mime)) = map.get("mimeType") else {
        return false;
    };
    let kind = media_kind(mime);
    let mut chosen = None;
    for (member, source) in SOURCES {
        match map.get(member) {
            None => {}
            Some(Value::String(_)) => {
                if chosen.is_none() {
                    chosen = Some((member, source));
                }
            }
            Some(_) => return false,
        }
    }
    let Some((member, source_type)) = chosen else {
        return false;
    };
    match map.get("filename") {
        None | Some(Value::String(_)) => {}
        Some(_) => return false,
    }

    let mime = super::remove_key(map, "mimeType").unwrap_or_default();
    let value = super::remove_key(map, member).unwrap_or_default();
    for (other, _) in SOURCES {
        if other != member && super::remove_key(map, other).is_some() {
            notes.lossy(DiagnosticCode::LegacyContent, &path.key(other), || {
                format!(
                    "legacy `binary` part named its bytes twice; upgraded to a `{kind}` part \
                     with the `{member}` source and dropped `{other}`; send one media part per \
                     source"
                )
            });
        }
    }
    map.insert("type".to_owned(), Value::String(kind.to_owned()));
    let mut source = Map::new();
    source.insert("type".to_owned(), Value::String(source_type.to_owned()));
    source.insert("value".to_owned(), value);
    source.insert("mimeType".to_owned(), mime);
    map.insert("source".to_owned(), Value::Object(source));
    if let Some(filename) = super::remove_key(map, "filename") {
        keep_filename(map, filename, path, notes);
    }

    // Lossless: reported once per run on received events only; an outgoing
    // input is upgraded in silence.
    if !notes.inbound {
        return true;
    }
    notes.notice(
        Notice::LegacyContent,
        DiagnosticCode::LegacyContent,
        path,
        || {
            format!(
                "legacy `binary` content part upgraded to a `{kind}` part; update the producer \
                 to send media parts (shim expires {})",
                super::SHIM_EXPIRY
            )
        },
    );
    true
}

/// Moves a legacy filename into the part's metadata, or warns when the part
/// already carries metadata that cannot take it.
fn keep_filename(
    map: &mut Map<String, Value>,
    filename: Value,
    path: &Path<'_>,
    notes: &mut Notes<'_, '_>,
) {
    match map.get_mut("metadata") {
        // A null metadata is absent (D9): the slot is free.
        None | Some(Value::Null) => {
            let mut metadata = Map::new();
            metadata.insert("filename".to_owned(), filename);
            map.insert("metadata".to_owned(), Value::Object(metadata));
        }
        Some(Value::Object(metadata)) if !metadata.contains_key("filename") => {
            metadata.insert("filename".to_owned(), filename);
        }
        Some(Value::Object(metadata)) if metadata.get("filename") == Some(&filename) => {}
        Some(_) => notes.lossy(DiagnosticCode::LegacyContent, &path.key("filename"), || {
            "legacy `binary` part's filename dropped: its metadata already holds another \
             value there; send media parts with the filename in metadata"
                .to_owned()
        }),
    }
}

/// The 1.0 part a MIME type maps to.
fn media_kind(mime: &str) -> &'static str {
    let top = mime.split('/').next().unwrap_or_default();
    if top.eq_ignore_ascii_case("image") {
        "image"
    } else if top.eq_ignore_ascii_case("audio") {
        "audio"
    } else if top.eq_ignore_ascii_case("video") {
        "video"
    } else {
        "document"
    }
}

/// Producer direction, for a reader that predates media parts (0.0.47 and
/// earlier): each media part with a well-formed source becomes a legacy
/// `binary` part. `data`, `url` and `file` sources map to `data`, `url` and
/// `id`; the MIME type moves up; a string `metadata.filename` becomes
/// `filename`. The legacy part requires a MIME type, so a part whose source
/// has none is dropped with a warning rather than given an invented one, and
/// anything else the legacy shape cannot carry (other metadata, a file
/// source's `provider`) is dropped with a warning. The part `id`, which
/// nothing reads, goes silently. A part with a malformed source is left as is.
pub(super) fn downgrade_parts(
    parts: &mut Vec<Value>,
    path: &Path<'_>,
    notes: &mut Notes<'_, '_>,
    peer: &'static str,
) {
    let parts_len = parts.len();
    let mut dropped: Vec<bool> = Vec::new();
    for (index, part) in parts.iter_mut().enumerate() {
        let Value::Object(map) = part else {
            continue;
        };
        let kind = match map.get("type").and_then(Value::as_str) {
            Some(kind @ ("image" | "audio" | "video" | "document")) => kind,
            _ => continue,
        };
        let Some(Value::Object(source)) = map.get("source") else {
            continue;
        };
        let member = match (source.get("type"), source.get("value")) {
            (Some(Value::String(t)), Some(Value::String(_))) => match t.as_str() {
                "data" => "data",
                "url" => "url",
                "file" => "id",
                _ => continue,
            },
            _ => continue,
        };
        let at = path.index(index);
        match source.get("mimeType") {
            Some(Value::String(_)) => {}
            None => {
                notes.lossy(DiagnosticCode::LossyDowngrade, &at, || {
                    format!(
                        "peer pinned to {peer} reads legacy binary parts only, which need a \
                         MIME type: dropped {kind} part without one; upgrade the peer to \
                         0.0.48 or later"
                    )
                });
                dropped.resize(parts_len, false);
                dropped[index] = true;
                continue;
            }
            Some(_) => continue,
        }
        let filename = match map.get("metadata") {
            None => None,
            Some(Value::Object(metadata)) if metadata.is_empty() => None,
            Some(Value::Object(metadata))
                if metadata.len() == 1
                    && matches!(metadata.get("filename"), Some(Value::String(_))) =>
            {
                metadata.get("filename").cloned()
            }
            Some(metadata) => {
                let filename = metadata.get("filename").filter(|f| f.is_string()).cloned();
                notes.lossy(DiagnosticCode::LossyDowngrade, &at.key("metadata"), || {
                    format!(
                        "peer pinned to {peer} reads legacy binary parts only: {kind} part \
                         metadata dropped; upgrade the peer to 0.0.48 or later"
                    )
                });
                filename
            }
        };
        if source.contains_key("provider") {
            notes.lossy(
                DiagnosticCode::LossyDowngrade,
                &at.key("source").key("provider"),
                || {
                    format!(
                        "peer pinned to {peer} reads legacy binary parts only: a file source's \
                     provider was dropped; upgrade the peer to 0.0.48 or later"
                    )
                },
            );
        }
        // The part is replaced below: move its source's members out instead
        // of copying them (both were checked above).
        let (mime, value) = match map.get_mut("source") {
            Some(Value::Object(source)) => (
                source.remove("mimeType").unwrap_or_default(),
                source.remove("value").unwrap_or_default(),
            ),
            _ => (Value::Null, Value::Null),
        };
        let mut legacy = Map::new();
        legacy.insert("type".to_owned(), Value::String("binary".to_owned()));
        legacy.insert("mimeType".to_owned(), mime);
        legacy.insert(member.to_owned(), value);
        if let Some(filename) = filename {
            legacy.insert("filename".to_owned(), filename);
        }
        *map = legacy;
    }
    if !dropped.is_empty() {
        let mut index = 0;
        parts.retain(|_| {
            let keep = !dropped[index];
            index += 1;
            keep
        });
    }
}

/// Producer direction for a caller that must keep typed content (0.0.47 and
/// earlier): each media part a legacy reader cannot take is dropped, with a
/// warning naming it. Nothing is invented in its place (VER-04).
pub(super) fn drop_parts(
    parts: &mut Vec<Value>,
    path: &Path<'_>,
    notes: &mut Notes<'_, '_>,
    peer: &'static str,
) {
    let mut index = 0;
    parts.retain(|part| {
        let at = path.index(index);
        index += 1;
        match part.get("type").and_then(Value::as_str) {
            Some(kind @ ("image" | "audio" | "video" | "document")) => {
                notes.lossy(DiagnosticCode::LossyDowngrade, &at, || {
                    format!(
                        "peer pinned to {peer} reads legacy binary parts only: dropped {kind} \
                         part; upgrade the peer to 0.0.48 or later"
                    )
                });
                false
            }
            _ => true,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::media_kind;

    #[test]
    fn mime_types_map_by_top_level_type() {
        assert_eq!(media_kind("image/png"), "image");
        assert_eq!(media_kind("IMAGE/JPEG"), "image");
        assert_eq!(media_kind("audio/wav"), "audio");
        assert_eq!(media_kind("video/mp4"), "video");
        assert_eq!(media_kind("application/pdf"), "document");
        assert_eq!(media_kind("text/plain"), "document");
        assert_eq!(media_kind("imagery/x"), "document");
        assert_eq!(media_kind(""), "document");
    }
}
