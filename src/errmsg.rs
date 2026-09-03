//! The message half of an error, without an allocation.
//!
//! Every message this crate raises for itself is a string literal, so the
//! natural representation is `&'static str`. The one thing that does not
//! fit is [`serde::de::Error::custom`], which takes `T: Display` and is
//! required by the trait: the text only exists at the moment it is
//! formatted.
//!
//! So [`Msg`] is a literal, plus an owned variant that exists only when
//! the `alloc` feature is on. Without an allocator a custom message
//! collapses to a fixed string. That is lossy, but it is lossy in the
//! text of an error and not in the verdict — a `Deserialize` impl that
//! rejected the input still rejects it, and on a target with no heap that
//! is the trade to make.
//!
//! `Box<str>` rather than `String`: the message is never appended to, and
//! it saves 8 bytes per error on a 64-bit target, 4 on a 32-bit one.

use core::fmt;

/// An error message: a literal, or an owned string when there is a heap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Msg {
    /// The usual case — a message the crate itself wrote.
    Static(&'static str),
    /// Text from `serde`'s `Error::custom`, which is only available as a
    /// `Display` and so has to be rendered somewhere.
    ///
    /// Both gates are load-bearing: `alloc` for somewhere to put the
    /// text, `serde` because nothing else in the crate produces a message
    /// it does not already own as a literal.
    #[cfg(all(feature = "alloc", feature = "serde"))]
    Custom(alloc::boxed::Box<str>),
}

impl Msg {
    /// Render a `serde` custom message.
    ///
    /// With `alloc`, this keeps the text. Without it, the text is
    /// discarded — see the module docs.
    #[cfg(feature = "serde")]
    pub(crate) fn custom<T: fmt::Display>(msg: T) -> Self {
        #[cfg(feature = "alloc")]
        {
            use alloc::string::ToString;
            Msg::Custom(msg.to_string().into_boxed_str())
        }
        #[cfg(not(feature = "alloc"))]
        {
            let _ = msg;
            Msg::Static("custom error")
        }
    }
}

impl fmt::Display for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Msg::Static(s) => f.write_str(s),
            #[cfg(all(feature = "alloc", feature = "serde"))]
            Msg::Custom(s) => f.write_str(s),
        }
    }
}

impl From<&'static str> for Msg {
    fn from(s: &'static str) -> Self {
        Msg::Static(s)
    }
}

/// [`serde::de::Unexpected`] rendered the way `serde_json` renders it,
/// without `core::fmt`'s float formatter.
///
/// Two divergences from serde's own `Display`, both of which
/// `serde_json` also makes, because this is a drop-in replacement:
///
/// * `Unit` prints as `null`, not `unit value`. In JSON it is `null`.
/// * `Float` is formatted by `zmij` rather than `{}`.
///
/// The second is the one that matters here. serde's impl prints an `f64`
/// with `{}`, which instantiates `core::fmt`'s shortest-round-trip float
/// formatter — `float_to_decimal_common_shortest` and `_exact` plus
/// grisu's `CACHED_POW10`, 11 848 bytes on `thumbv7em-none-eabihf`,
/// reached only through an error message on a chip with no
/// double-precision FPU. `zmij` is already a dependency, formats into a
/// stack buffer, and produces the same digits, so the text is unchanged
/// and the cost is a tenth of it. See `docs/SIZE.md`.
///
/// Every arm is spelled out rather than delegating the rest to serde:
/// calling `Display for Unexpected` at all links the whole function,
/// float arm included. `serde_json` delegates and relies on the
/// optimiser proving the float arm dead; being explicit does not need
/// that to work. The match is exhaustive on purpose, so a new serde
/// variant is a compile error rather than a silently worse message.
#[cfg(feature = "serde")]
pub(crate) struct Unexpected<'a>(pub(crate) serde::de::Unexpected<'a>);

#[cfg(feature = "serde")]
impl fmt::Display for Unexpected<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use serde::de::Unexpected as U;
        match self.0 {
            U::Bool(b) => write!(f, "boolean `{b}`"),
            U::Unsigned(i) => write!(f, "integer `{i}`"),
            U::Signed(i) => write!(f, "integer `{i}`"),
            U::Float(v) => write!(f, "floating point `{}`", zmij::Buffer::new().format(v)),
            U::Char(c) => write!(f, "character `{c}`"),
            U::Str(s) => write!(f, "string {s:?}"),
            U::Bytes(_) => f.write_str("byte array"),
            U::Unit => f.write_str("null"),
            U::Option => f.write_str("Option value"),
            U::NewtypeStruct => f.write_str("newtype struct"),
            U::Seq => f.write_str("sequence"),
            U::Map => f.write_str("map"),
            U::Enum => f.write_str("enum"),
            U::UnitVariant => f.write_str("unit variant"),
            U::NewtypeVariant => f.write_str("newtype variant"),
            U::TupleVariant => f.write_str("tuple variant"),
            U::StructVariant => f.write_str("struct variant"),
            U::Other(other) => f.write_str(other),
        }
    }
}
