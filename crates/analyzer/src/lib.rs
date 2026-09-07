pub mod analysis;
pub mod audio;
pub mod emu;
pub mod export;
pub mod header;
pub mod playerid;
pub mod songlengths;
pub mod stil;
pub mod support;
pub mod trace;

/// Define a `pub struct $name(pub $inner)` newtype with a `Display` impl using
/// the supplied format string (which receives `self.0`). Also derives
/// `serde::Serialize` with `#[serde(transparent)]` so JSON output is the
/// raw numeric value, not a wrapper object.
macro_rules! hex_newtype {
    ($name:ident, $inner:ty, $fmt:literal) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Default,
            ::serde::Serialize,
            ::serde::Deserialize,
        )]
        #[serde(transparent)]
        #[must_use]
        pub struct $name(pub $inner);

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                ::std::write!(f, $fmt, self.0)
            }
        }
    };
}
pub(crate) use hex_newtype;
