//! `serde::Deserializer` over the node pool.
//!
//! This is what makes a fair struct-level comparison possible: the same
//! `#[derive(Deserialize)]` type can be filled from `serde_json`,
//! `simd-json`, `sonic-rs` or this pool, so the benchmark measures the
//! parser rather than the glue.
//!
//! # Why the pool is a good source for this
//!
//! Deserialising into a struct reads each field exactly once and discards
//! the document. That is the access pattern the tier-3 representation is
//! best at:
//!
//! * strings are `(offset, len)` into the input, so a field that
//!   deserialises to `&str` or `Cow<str>` **borrows** with no copy
//!   whenever it contains no escapes;
//! * numbers are already decoded in the node payload;
//! * skipping an unknown field is `skip_subtree`, an index walk with no
//!   byte scanning at all.
//!
//! # Numbers
//!
//! Use [`crate::strict::StrictParser`] as the source. The faithful parser
//! is integer-only and would deserialise `3.14` as `314`
//! (see [`crate::scalar`]); [`Deserializer::from_faithful`] exists but is
//! marked accordingly.

use crate::pool::Pool;
use crate::query::{Doc, Value};
use crate::tag::Type;
use serde::de::{
    self, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};
use serde::forward_to_deserialize_any;
use std::borrow::Cow;
use std::fmt;

/// Deserialisation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    msg: String,
}

impl Error {
    fn new(msg: impl Into<String>) -> Self {
        Error { msg: msg.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::new(msg.to_string())
    }
}

type Result<T> = core::result::Result<T, Error>;

/// A `serde` deserializer reading one pool node.
#[derive(Debug, Clone, Copy)]
pub struct Deserializer<'de> {
    value: Value<'de>,
}

impl<'de> Deserializer<'de> {
    /// Deserialize starting at a document root.
    #[must_use]
    pub fn from_doc(doc: Doc<'de>) -> Self {
        Deserializer { value: doc.root() }
    }

    /// Deserialize starting at an arbitrary node.
    #[must_use]
    pub fn from_value(value: Value<'de>) -> Self {
        Deserializer { value }
    }

    /// Pair an input with a pool built by the **faithful** parser.
    ///
    /// Remember that parser is integer-only and does not validate; prefer
    /// [`from_strict`] unless you are deliberately measuring it.
    #[must_use]
    pub fn from_faithful(input: &'de [u8], pool: &'de Pool) -> Self {
        Self::from_doc(Doc::new(input, pool))
    }
}

/// Parse with the strict parser and deserialize into an owned `T`.
///
/// `T` must be [`serde::de::DeserializeOwned`]: the node pool lives only
/// for the call, and [`Doc`] ties the input's lifetime to the pool's, so a
/// borrowing `T` cannot outlive it.
///
/// To deserialize a *borrowing* type, keep a
/// [`crate::strict::StrictParser`] and call [`from_doc`] on its result —
/// the borrow checker then relates the lifetimes correctly and nothing is
/// copied.
///
/// An earlier version accepted a borrowing `T` by leaking the pool with
/// `Box::leak`. That is fine in a test and unacceptable in a library: it
/// leaks on every call.
pub fn from_slice<T>(input: &[u8]) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let pool = crate::strict::parse_to_pool(input)
        .map_err(|e| Error::new(format!("parse error: {e}")))?;
    T::deserialize(Deserializer::from_doc(Doc::new(input, &pool)))
}

/// Deserialize `T` from an already-parsed document.
pub fn from_doc<'de, T>(doc: Doc<'de>) -> Result<T>
where
    T: serde::Deserialize<'de>,
{
    T::deserialize(Deserializer::from_doc(doc))
}

impl<'de> de::Deserializer<'de> for Deserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let v = self.value;
        match v.typ() {
            Type::Null => visitor.visit_unit(),
            Type::Bool => visitor.visit_bool(v.as_bool().unwrap_or(false)),
            Type::Number => visitor.visit_i64(v.as_i64().unwrap_or(0)),
            Type::Float => visitor.visit_f64(v.as_f64().unwrap_or(0.0)),
            Type::String | Type::Key => visit_str(v, visitor),
            Type::Array => visitor.visit_seq(SeqReader {
                iter: v.elements(),
            }),
            Type::Object => visitor.visit_map(MapReader {
                iter: v.entries(),
                pending: None,
            }),
        }
    }

    /// `Option` is the one place the pool's `null` needs special handling:
    /// serde must be told *before* it asks for the inner type.
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        if self.value.is_null() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self.value.typ() {
            // `"Variant"` — a unit variant.
            Type::String => {
                let s = self
                    .value
                    .as_str()
                    .ok_or_else(|| Error::new("enum variant is not valid UTF-8"))?;
                visitor.visit_enum(UnitVariant { name: s })
            }
            // `{"Variant": payload}` — exactly one pair.
            Type::Object => {
                let mut it = self.value.entries();
                let Some((k, v)) = it.next() else {
                    return Err(Error::new("enum object must have exactly one key"));
                };
                if it.next().is_some() {
                    return Err(Error::new("enum object must have exactly one key"));
                }
                let name = crate::unescape::unescape(k)
                    .ok_or_else(|| Error::new("enum variant key is not valid UTF-8"))?;
                visitor.visit_enum(PayloadVariant { name, value: v })
            }
            other => Err(Error::new(format!("cannot deserialize enum from {other:?}"))),
        }
    }

    // Everything else is structurally determined by the node type, so
    // `deserialize_any` is correct and the hints add nothing.
    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit seq tuple tuple_struct map struct identifier
        ignored_any
    }
}

/// Hand a string to the visitor, borrowing from the input when it contains
/// no escapes.
fn visit_str<'de, V: Visitor<'de>>(v: Value<'de>, visitor: V) -> Result<V::Value> {
    let raw = v
        .as_raw_str()
        .ok_or_else(|| Error::new("expected a string node"))?;
    match crate::unescape::unescape(raw) {
        // Zero-copy: the common case.
        Some(Cow::Borrowed(s)) => visitor.visit_borrowed_str(s),
        Some(Cow::Owned(s)) => visitor.visit_string(s),
        None => Err(Error::new("invalid escape sequence or UTF-8 in string")),
    }
}

struct SeqReader<'de> {
    iter: crate::query::Elements<'de>,
}

impl<'de> SeqAccess<'de> for SeqReader<'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        match self.iter.next() {
            Some(v) => seed.deserialize(Deserializer::from_value(v)).map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.size_hint().0)
    }
}

struct MapReader<'de> {
    iter: crate::query::Entries<'de>,
    pending: Option<Value<'de>>,
}

impl<'de> MapAccess<'de> for MapReader<'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        let Some((raw, val)) = self.iter.next() else {
            return Ok(None);
        };
        self.pending = Some(val);
        let key = crate::unescape::unescape(raw)
            .ok_or_else(|| Error::new("object key is not valid UTF-8"))?;
        match key {
            Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s)).map(Some),
            Cow::Owned(s) => seed.deserialize(s.into_deserializer()).map(Some),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let Some(v) = self.pending.take() else {
            return Err(Error::new("next_value_seed called before next_key_seed"));
        };
        seed.deserialize(Deserializer::from_value(v))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.size_hint().0)
    }
}

/// A borrowed `&'de str` as a deserializer.
///
/// `serde`'s own `StrDeserializer` does not preserve the `'de` lifetime for
/// `visit_borrowed_str`, and field-name matching in derived impls is much
/// faster when the key can be borrowed.
struct BorrowedStr<'de>(&'de str);

impl<'de> de::Deserializer<'de> for BorrowedStr<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_str(self.0)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

struct UnitVariant<'de> {
    name: Cow<'de, str>,
}

impl<'de> EnumAccess<'de> for UnitVariant<'de> {
    type Error = Error;
    type Variant = UnitPayload;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, UnitPayload)> {
        let v = match self.name {
            Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s))?,
            Cow::Owned(s) => seed.deserialize(s.into_deserializer())?,
        };
        Ok((v, UnitPayload))
    }
}

struct UnitPayload;

impl<'de> VariantAccess<'de> for UnitPayload {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, _seed: T) -> Result<T::Value> {
        Err(Error::new("expected a unit variant"))
    }
    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, _visitor: V) -> Result<V::Value> {
        Err(Error::new("expected a unit variant"))
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value> {
        Err(Error::new("expected a unit variant"))
    }
}

struct PayloadVariant<'de> {
    name: Cow<'de, str>,
    value: Value<'de>,
}

impl<'de> EnumAccess<'de> for PayloadVariant<'de> {
    type Error = Error;
    type Variant = Payload<'de>;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Payload<'de>)> {
        let v = match self.name {
            Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s))?,
            Cow::Owned(s) => seed.deserialize(s.into_deserializer())?,
        };
        Ok((v, Payload { value: self.value }))
    }
}

struct Payload<'de> {
    value: Value<'de>,
}

impl<'de> VariantAccess<'de> for Payload<'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        seed.deserialize(Deserializer::from_value(self.value))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        visitor.visit_seq(SeqReader {
            iter: self.value.elements(),
        })
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_map(MapReader {
            iter: self.value.entries(),
            pending: None,
        })
    }
}
