//! Declares the crate's "sparse" wire records: camelCase objects whose
//! members are all optional, read leniently (a missing member is `None`) and
//! written compactly (a `None` member is left out rather than sent as `null`).
//!
//! Writing the attributes once here keeps every such record identical in how
//! it treats absence, which several spec rules depend on (an omitted
//! declaration is "undeclared", an unreported count is not a zero).

/// Expands to a `#[non_exhaustive]` struct whose every field is
/// `Option<_>`, defaulted on read and skipped on write when absent. Extra
/// per-field attributes (docs, `#[serde(rename = ...)]`) pass through.
macro_rules! sparse_record {
    (
        $(#[$outer:meta])*
        pub struct $record:ident {
            $(
                $(#[$inner:meta])*
                pub $member:ident: Option<$kind:ty>,
            )*
        }
    ) => {
        $(#[$outer])*
        #[derive(Debug, Clone, Default, PartialEq, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        #[non_exhaustive]
        pub struct $record {
            $(
                $(#[$inner])*
                #[serde(default, skip_serializing_if = "Option::is_none")]
                pub $member: Option<$kind>,
            )*
        }
    };
}
