//! [`JsonObject`]: an open JSON object.
//!
//! The schema leaves some objects open: `metadata`, activity `content`,
//! interrupt `responseSchema`, capability `custom`, and JSON Patch operations.
//! Open means every member survives enforcement, including members this SDK
//! does not recognise and `null` values (BASIC-11, PROC-15, META-01). Those
//! positions use this type rather than `serde_json::Map`, so the map type
//! (and its feature-dependent ordering) is not part of this crate's API.
//!
//! Member order follows `serde_json`'s map: sorted by default, insertion order
//! with this crate's `preserve-order` feature (which enables
//! `serde_json/preserve_order`; feature unification then applies it to every
//! crate in the build). Byte-exact protobuf parity from *typed* events needs
//! that feature; from JSON text it never does. See [`crate::proto`].
//! Equality ignores order either way.
//!
//! # Absent versus `null`
//!
//! An optional open-JSON member (`rawEvent`, `result`, `payload`,
//! `parameters`, part `metadata`, `state`, `forwardedProps`) is
//! `Option<Value>`. `None` and `Some(Value::Null)` both serialize as *absent*
//! (BASIC-09: optional fields are omitted, never `null`), and the `with_*`
//! setters normalise `Null` to `None`. A member whose `null` is meaningful
//! (JSON Patch `value`) uses the "present" rule instead: a present `null`
//! deserializes as `Some(Value::Null)` and serializes back as `null`.
#![cfg_attr(
    not(feature = "proto"),
    doc = "",
    doc = "[`crate::proto`]: https://docs.rs/agui-sdk/latest/agui_sdk/proto/index.html"
)]

use std::fmt;
use std::marker::PhantomData;

use serde::de::value::{MapAccessDeserializer, StringDeserializer};
use serde::de::{self, DeserializeOwned, DeserializeSeed, IntoDeserializer, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// An open JSON object: string keys, any JSON values, `null` included.
///
/// Serializes as a plain JSON object. Equality is member-wise and ignores
/// order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JsonObject(Map<String, Value>);

impl JsonObject {
    /// An empty object.
    #[must_use]
    pub fn new() -> Self {
        Self(Map::new())
    }

    /// The value under `key`, if present. A present `null` is `Some(&Value::Null)`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Mutable access to the value under `key`.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.0.get_mut(key)
    }

    /// Whether `key` is present (with any value, `null` included).
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// Inserts or replaces a member, returning the previous value.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        self.0.insert(key.into(), value.into())
    }

    /// Removes a member, returning its value. With `preserve-order` the last
    /// member may move into the vacated position.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.0.remove(key)
    }

    /// Number of members.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the object has no members.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates members in map order.
    #[must_use]
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            inner: self.0.iter(),
        }
    }

    /// Shallow, key-by-key merge with last-write-wins: every member of `newer`
    /// replaces the member of the same key here, objects and arrays wholesale,
    /// never recursively (META-06, META-07). Cost is O(members of `newer`);
    /// `newer`'s storage is reused, nothing is cloned.
    pub fn merge_from(&mut self, newer: JsonObject) {
        for (key, value) in newer.0 {
            self.0.insert(key, value);
        }
    }

    /// Converts into a [`Value::Object`]. No copy.
    #[must_use]
    pub fn into_value(self) -> Value {
        Value::Object(self.0)
    }

    /// Takes the object out of a [`Value`], or gives the value back if it is
    /// not an object. No copy.
    pub fn from_value(value: Value) -> Result<Self, Value> {
        match value {
            Value::Object(map) => Ok(Self(map)),
            other => Err(other),
        }
    }
}

impl From<JsonObject> for Value {
    fn from(object: JsonObject) -> Self {
        Value::Object(object.0)
    }
}

impl FromIterator<(String, Value)> for JsonObject {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        Self(Map::from_iter(iter))
    }
}

/// Iterator over a [`JsonObject`]'s members, from [`JsonObject::iter`].
#[derive(Debug)]
pub struct Iter<'a> {
    inner: serde_json::map::Iter<'a>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a str, &'a Value);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(key, value)| (key.as_str(), value))
    }
}

/// A union discriminated by one string member (`type` for events, content
/// parts, sources and outcomes; `role` for messages).
///
/// serde's derived internally tagged enums buffer the whole object into an
/// intermediate tree before choosing a variant. [`deserialize_tagged`] does
/// not: members that precede the tag are held as `(String, Value)` pairs, the
/// tag picks the variant, and the variant's own `Deserialize` reads the held
/// pairs followed by the rest of the live input. When the tag is the first
/// member, which is how this crate and the reference SDKs write events,
/// nothing is buffered at all.
pub(crate) trait Tagged: Sized {
    /// Name of the discriminating member.
    const TAG: &'static str;
    /// What the visitor expects, for error messages.
    const EXPECTING: &'static str;
    /// The discriminator's type: a closed enum of wire spellings.
    type Tag: DeserializeOwned;

    /// Builds the variant named by `tag` from the object's other members.
    fn from_tag<'de, D: Deserializer<'de>>(tag: Self::Tag, rest: D) -> Result<Self, D::Error>;
}

/// Deserializes a [`Tagged`] union without buffering the object.
pub(crate) fn deserialize_tagged<'de, T: Tagged, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    deserializer.deserialize_map(TaggedVisitor(PhantomData))
}

struct TaggedVisitor<T>(PhantomData<T>);

impl<'de, T: Tagged> Visitor<'de> for TaggedVisitor<T> {
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(T::EXPECTING)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<T, A::Error> {
        let mut held: Vec<(String, Value)> = Vec::new();
        loop {
            match map.next_key_seed(KeySeed(T::TAG))? {
                None => return Err(de::Error::missing_field(T::TAG)),
                Some(Key::Tag) => {
                    let tag: T::Tag = map.next_value()?;
                    let rest = HeldThenLive {
                        held: held.into_iter(),
                        pending: None,
                        live: map,
                    };
                    return T::from_tag(tag, MapAccessDeserializer::new(rest));
                }
                Some(Key::Other(key)) => {
                    let value: Value = map.next_value()?;
                    held.push((key, value));
                }
            }
        }
    }
}

enum Key {
    Tag,
    Other(String),
}

struct KeySeed(&'static str);

impl<'de> DeserializeSeed<'de> for KeySeed {
    type Value = Key;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Key, D::Error> {
        deserializer.deserialize_identifier(self)
    }
}

impl<'de> Visitor<'de> for KeySeed {
    type Value = Key;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a member name")
    }

    fn visit_str<E: de::Error>(self, key: &str) -> Result<Key, E> {
        if key == self.0 {
            Ok(Key::Tag)
        } else {
            Ok(Key::Other(key.to_owned()))
        }
    }

    fn visit_string<E: de::Error>(self, key: String) -> Result<Key, E> {
        if key == self.0 {
            Ok(Key::Tag)
        } else {
            Ok(Key::Other(key))
        }
    }
}

/// Replays the members read before the tag, then continues with the input.
struct HeldThenLive<A> {
    held: std::vec::IntoIter<(String, Value)>,
    pending: Option<Value>,
    live: A,
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for HeldThenLive<A> {
    type Error = A::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, A::Error> {
        if let Some((key, value)) = self.held.next() {
            self.pending = Some(value);
            let key: StringDeserializer<A::Error> = key.into_deserializer();
            return seed.deserialize(key).map(Some);
        }
        self.live.next_key_seed(seed)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, A::Error> {
        match self.pending.take() {
            Some(value) => seed.deserialize(value).map_err(de::Error::custom),
            None => self.live.next_value_seed(seed),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        self.live
            .size_hint()
            .map(|live| live + self.held.len() + usize::from(self.pending.is_some()))
    }
}

/// Deserializes an object `value` with its `tag` member presented first, so
/// a [`Tagged`] union built from a `Value` reads its discriminator before
/// anything else and holds nothing back. With the default sorted map the tag
/// is rarely first (`"delta"`, `"messageId"` < `"type"`); moving it costs a
/// map removal, where buffering the members before it costs a `Vec`.
pub(crate) fn from_value_tag_first<T: DeserializeOwned>(
    value: Value,
    tag: &str,
) -> Result<T, serde_json::Error> {
    match value {
        // Already first (insertion order from a producer that writes the
        // tag first): nothing to move.
        Value::Object(map) if map.keys().next().map(String::as_str) == Some(tag) => {
            T::deserialize(Value::Object(map))
        }
        // Not first: take it out. (With `preserve_order` the removal may
        // move another member into its slot; the members after the tag are
        // read by name, so their order is immaterial.)
        Value::Object(mut map) => match map.remove_entry(tag) {
            Some(entry) => T::deserialize(MapAccessDeserializer::new(TagFirst {
                tag: Some(entry),
                pending: None,
                rest: map.into_iter(),
            })),
            None => T::deserialize(Value::Object(map)),
        },
        other => T::deserialize(other),
    }
}

/// The members of an object, its discriminator first.
struct TagFirst {
    tag: Option<(String, Value)>,
    pending: Option<Value>,
    rest: serde_json::map::IntoIter,
}

impl<'de> MapAccess<'de> for TagFirst {
    type Error = serde_json::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let Some((key, value)) = self.tag.take().or_else(|| self.rest.next()) else {
            return Ok(None);
        };
        self.pending = Some(value);
        let key: StringDeserializer<Self::Error> = key.into_deserializer();
        seed.deserialize(key).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        match self.pending.take() {
            Some(value) => seed.deserialize(value),
            None => Err(de::Error::custom("value requested before its key")),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.rest.len() + usize::from(self.tag.is_some()))
    }
}

/// Consumes whatever members remain, for variants that carry none.
pub(crate) fn skip_rest<'de, D: Deserializer<'de>>(rest: D) -> Result<(), D::Error> {
    de::IgnoredAny::deserialize(rest).map(|_| ())
}

/// `skip_serializing_if` predicate for optional open JSON: `None` and
/// `Some(Null)` are both absent on the wire (BASIC-09, ARCH-04).
pub(crate) fn is_absent(value: &Option<Value>) -> bool {
    matches!(value, None | Some(Value::Null))
}

/// `deserialize_with` for a member where a present `null` is a value: a
/// present member (even `null`) is `Some`; an absent one stays `None` through
/// `#[serde(default)]`.
// Kept, unused today, for any future member where a present `null` is a value
// (docs/API.md lists it as provided for such members).
#[allow(dead_code)]
pub(crate) fn deserialize_present<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// Optional open JSON as a setter stores it: `null` is absence.
pub(crate) fn present(value: Value) -> Option<Value> {
    match value {
        Value::Null => None,
        other => Some(other),
    }
}

/// The serialized size of `value`, counted without building it.
pub(crate) fn wire_bytes<T: Serialize + ?Sized>(value: &T) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buf.len());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    // Serializing into a counter cannot fail for these types.
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

/// Estimated heap bytes `value` owns once held as this build's types (64-bit,
/// the `serde_json` [`Value`] layout compiled in), counted without building
/// anything: the unit of [`Limits::max_history_bytes`].
///
/// The model, per serialized item:
/// - a string, key or byte string of `n > 0` bytes: `n` rounded up to 16 (its
///   heap block; the `String` header lives in its parent's slot);
/// - a sequence of `n` elements: `slots(n)` [`Value`] slots, plus each
///   element (`slots` models capacity slack where it is resident: a power of
///   two of at least 4 up to 128 elements, exactly `n` beyond, where the
///   untouched tail of a large block is not resident);
/// - a map of `n` entries: its node or table storage ([`map_bytes`]), plus
///   each key and value (the value's slot is inside the node);
/// - a struct, tuple, `Option`, newtype or enum variant: its fields only
///   (they are inline); a scalar: nothing.
///
/// [`Limits::max_history_bytes`]: crate::pipeline::Limits::max_history_bytes
pub(crate) fn resident_bytes<T: Serialize + ?Sized>(value: &T) -> usize {
    let mut total = 0;
    // Counting cannot fail: no method below returns an error.
    let _ = value.serialize(resident::Resident(&mut total));
    total
}

/// Bytes of one `Value` slot.
pub(crate) const VALUE_SLOT: usize = std::mem::size_of::<Value>();

/// The slot a value written at member or element `key` of `parent` takes
/// (`parent` is `None` at the root), as what the container's own storage
/// grows by, so charges for entries added one at a time sum to the charge
/// for the whole container: in an object, a new member's key heap and
/// [`map_bytes`] of one more entry, nothing for an existing member (the slot
/// is reused); in an array, [`seq_bytes`] of one more element; at the root,
/// nothing.
pub(crate) fn slot_bytes(parent: Option<&Value>, key: &str) -> usize {
    match parent {
        Some(Value::Object(m)) if !m.contains_key(key) => member_slot_bytes(m.len(), key.len()),
        Some(Value::Array(a)) => element_slot_bytes(a.len()),
        _ => 0,
    }
}

/// [`slot_bytes`] of a new member with a key of `key_len` bytes in an object
/// of `len` members: its storage at one more entry, and the key's heap.
pub(crate) fn member_slot_bytes(len: usize, key_len: usize) -> usize {
    map_bytes(len + 1)
        .saturating_sub(map_bytes(len))
        .saturating_add(text_bytes(key_len))
}

/// [`slot_bytes`] of a new element in an array of `len` elements.
pub(crate) fn element_slot_bytes(len: usize) -> usize {
    seq_bytes(len + 1).saturating_sub(seq_bytes(len))
}

/// Slot bytes of a sequence of `n` elements: `slots(n)` [`Value`] slots.
/// Never falls as `n` grows.
pub(crate) fn seq_bytes(n: usize) -> usize {
    slots(n).saturating_mul(VALUE_SLOT)
}

const ENTRY: usize = std::mem::size_of::<String>() + VALUE_SLOT;

/// A B-tree leaf of `serde_json`'s default map: 11 entries and a header.
const BTREE_NODE: usize = 11 * ENTRY + 16;

/// The heap block of a string of `len` bytes.
pub(crate) fn text_bytes(len: usize) -> usize {
    if len == 0 {
        0
    } else {
        len.saturating_add(15) & !15
    }
}

/// `Value` slots a sequence of `n` elements is charged.
fn slots(n: usize) -> usize {
    match n {
        0 => 0,
        1..=128 => n.max(4).next_power_of_two(),
        _ => n,
    }
}

/// Whether `serde_json`'s map is an `IndexMap` in this build: its
/// `preserve_order` feature can be switched on by any crate in the graph, so
/// the layout is read from the compiled types, not from this crate's
/// features.
const INDEXED: bool = std::mem::size_of::<Map<String, Value>>()
    != std::mem::size_of::<std::collections::BTreeMap<String, Value>>();

/// Node or table bytes of a `serde_json` map of `n` entries, keys' and
/// values' own heap excluded. Never falls as `n` grows.
pub(crate) fn map_bytes(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    if INDEXED {
        // The index table (one `usize` and one control byte per bucket, and
        // a control group), and the entries vector (each with its hash),
        // which `IndexMap` keeps at the table's capacity.
        let buckets = if n < 4 {
            4
        } else if n < 8 {
            8
        } else {
            n.saturating_mul(8).div_ceil(7).next_power_of_two()
        };
        let capacity = if buckets < 8 {
            buckets - 1
        } else {
            buckets / 8 * 7
        };
        capacity
            .saturating_mul(ENTRY + 8)
            .saturating_add(buckets.saturating_mul(9))
            .saturating_add(16)
    } else {
        // B-tree leaves of 11 entries, about half full for the ascending
        // inserts `serde_json` and patches make.
        n.div_ceil(6).saturating_mul(BTREE_NODE)
    }
}

mod resident {
    use serde::ser::{self, Serialize};

    use super::{map_bytes, seq_bytes, text_bytes};

    type Error = serde_json::Error;

    /// The counting serializer behind [`super::resident_bytes`].
    pub(super) struct Resident<'a>(pub(super) &'a mut usize);

    impl Resident<'_> {
        fn add(&mut self, n: usize) {
            *self.0 = self.0.saturating_add(n);
        }
    }

    /// A sequence or map in progress: its entries are counted on `end`.
    pub(super) struct Container<'a> {
        total: &'a mut usize,
        entries: usize,
        map: bool,
    }

    impl Container<'_> {
        fn element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
            value.serialize(Resident(self.total))
        }

        fn finish(self) {
            let n = if self.map {
                map_bytes(self.entries)
            } else {
                seq_bytes(self.entries)
            };
            *self.total = self.total.saturating_add(n);
        }
    }

    impl<'a> ser::Serializer for Resident<'a> {
        type Ok = ();
        type Error = Error;
        type SerializeSeq = Container<'a>;
        type SerializeTuple = Self;
        type SerializeTupleStruct = Self;
        type SerializeTupleVariant = Self;
        type SerializeMap = Container<'a>;
        type SerializeStruct = Self;
        type SerializeStructVariant = Self;

        fn serialize_bool(self, _: bool) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_i8(self, _: i8) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_i16(self, _: i16) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_i32(self, _: i32) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_i64(self, _: i64) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_u8(self, _: u8) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_u16(self, _: u16) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_u32(self, _: u32) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_u64(self, _: u64) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_f32(self, _: f32) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_f64(self, _: f64) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_char(self, _: char) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_str(mut self, v: &str) -> Result<(), Error> {
            self.add(text_bytes(v.len()));
            Ok(())
        }
        fn serialize_bytes(mut self, v: &[u8]) -> Result<(), Error> {
            self.add(text_bytes(v.len()));
            Ok(())
        }
        fn serialize_none(self) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Error> {
            value.serialize(self)
        }
        fn serialize_unit(self) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_unit_struct(self, _: &'static str) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_unit_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
        ) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_newtype_struct<T: Serialize + ?Sized>(
            self,
            _: &'static str,
            value: &T,
        ) -> Result<(), Error> {
            value.serialize(self)
        }
        fn serialize_newtype_variant<T: Serialize + ?Sized>(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            value: &T,
        ) -> Result<(), Error> {
            value.serialize(self)
        }
        fn serialize_seq(self, _: Option<usize>) -> Result<Container<'a>, Error> {
            Ok(Container {
                total: self.0,
                entries: 0,
                map: false,
            })
        }
        fn serialize_tuple(self, _: usize) -> Result<Self, Error> {
            Ok(self)
        }
        fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self, Error> {
            Ok(self)
        }
        fn serialize_tuple_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self, Error> {
            Ok(self)
        }
        fn serialize_map(self, _: Option<usize>) -> Result<Container<'a>, Error> {
            Ok(Container {
                total: self.0,
                entries: 0,
                map: true,
            })
        }
        fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self, Error> {
            Ok(self)
        }
        fn serialize_struct_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self, Error> {
            Ok(self)
        }
    }

    impl ser::SerializeSeq for Container<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
            self.entries += 1;
            self.element(value)
        }
        fn end(self) -> Result<(), Error> {
            self.finish();
            Ok(())
        }
    }

    impl ser::SerializeMap for Container<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
            self.entries += 1;
            self.element(key)
        }
        fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
            self.element(value)
        }
        fn end(self) -> Result<(), Error> {
            self.finish();
            Ok(())
        }
    }

    impl ser::SerializeTuple for Resident<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
            value.serialize(Resident(self.0))
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ser::SerializeTupleStruct for Resident<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
            value.serialize(Resident(self.0))
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ser::SerializeTupleVariant for Resident<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
            value.serialize(Resident(self.0))
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ser::SerializeStruct for Resident<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_field<T: Serialize + ?Sized>(
            &mut self,
            _: &'static str,
            value: &T,
        ) -> Result<(), Error> {
            value.serialize(Resident(self.0))
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ser::SerializeStructVariant for Resident<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_field<T: Serialize + ?Sized>(
            &mut self,
            _: &'static str,
            value: &T,
        ) -> Result<(), Error> {
            value.serialize(Resident(self.0))
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }
}
