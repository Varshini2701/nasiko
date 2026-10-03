use thiserror::Error;

/// Every error this crate surfaces.
///
/// Note what is absent: compression failures. [`crate::compress`] is infallible by design — a
/// malformed payload is returned unchanged, not reported — so there is no `Json(..)` variant.
/// The only fallible operations are parsing a caller's config strings.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompressError {
    #[error(
        "unknown content type '{0}' (expected one of: json, log, diff, search, code, markup, prose)"
    )]
    UnknownContentType(String),

    #[error(
        "unknown compression level '{0}' (expected one of: conservative, balanced, aggressive)"
    )]
    UnknownLevel(String),
}
