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
    #[cfg(feature = "alloc")]
    Custom(alloc::boxed::Box<str>),
}

impl Msg {
    /// Render a `serde` custom message.
    ///
    /// With `alloc`, this keeps the text. Without it, the text is
    /// discarded — see the module docs.
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
            #[cfg(feature = "alloc")]
            Msg::Custom(s) => f.write_str(s),
        }
    }
}

impl From<&'static str> for Msg {
    fn from(s: &'static str) -> Self {
        Msg::Static(s)
    }
}
