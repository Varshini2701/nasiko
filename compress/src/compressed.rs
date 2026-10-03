use std::borrow::Cow;

use crate::content_type::ContentType;

/// The result of a [`crate::compress`] call.
///
/// Borrows the input when nothing was compressed. That matters because callers run this over
/// every message of every request and most fall below `min_bytes` — the no-op path must not
/// allocate.
#[derive(Debug, Clone)]
pub struct Compressed<'a> {
    text: Cow<'a, str>,
    content_type: ContentType,
    original_bytes: usize,
    projected_bytes: usize,
}

impl<'a> Compressed<'a> {
    pub(crate) fn unchanged(input: &'a str, content_type: ContentType) -> Self {
        Self {
            text: Cow::Borrowed(input),
            content_type,
            original_bytes: input.len(),
            projected_bytes: input.len(),
        }
    }

    pub(crate) fn shrunk(input: &'a str, content_type: ContentType, out: String) -> Self {
        Self {
            original_bytes: input.len(),
            projected_bytes: out.len(),
            text: Cow::Owned(out),
            content_type,
        }
    }

    /// Dry run: report what the output *would* have been, return the input untouched.
    pub(crate) fn projected(
        input: &'a str,
        content_type: ContentType,
        projected_bytes: usize,
    ) -> Self {
        Self {
            text: Cow::Borrowed(input),
            content_type,
            original_bytes: input.len(),
            projected_bytes,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text.into_owned()
    }

    pub fn content_type(&self) -> ContentType {
        self.content_type
    }

    pub fn original_bytes(&self) -> usize {
        self.original_bytes
    }

    /// Size of what this actually carries.
    pub fn compressed_bytes(&self) -> usize {
        self.text.len()
    }

    /// Size the compressed form would occupy. Equal to [`Self::compressed_bytes`] unless the
    /// policy was a dry run.
    pub fn projected_bytes(&self) -> usize {
        self.projected_bytes
    }

    pub fn saved_bytes(&self) -> usize {
        self.original_bytes.saturating_sub(self.projected_bytes)
    }

    /// Whether the carried text differs from the input.
    ///
    /// A strict length comparison rather than a `Cow::Owned` check, so a compressor returning an
    /// owned-but-identical string cannot misreport — and so this predicate and the never-grows
    /// invariant cannot drift apart.
    pub fn is_changed(&self) -> bool {
        self.text.len() < self.original_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_borrows_and_reports_no_saving() {
        let c = Compressed::unchanged("hello", ContentType::Prose);
        assert!(matches!(c.text, Cow::Borrowed(_)));
        assert!(!c.is_changed());
        assert_eq!(c.saved_bytes(), 0);
        assert_eq!(c.compressed_bytes(), c.original_bytes());
    }

    #[test]
    fn shrunk_reports_the_saving() {
        let c = Compressed::shrunk("hello world", ContentType::Prose, "hi".into());
        assert!(c.is_changed());
        assert_eq!(c.original_bytes(), 11);
        assert_eq!(c.compressed_bytes(), 2);
        assert_eq!(c.saved_bytes(), 9);
    }

    #[test]
    fn dry_run_reports_a_saving_while_carrying_the_original() {
        let c = Compressed::projected("hello world", ContentType::Prose, 2);
        assert_eq!(c.text(), "hello world");
        assert!(!c.is_changed());
        assert_eq!(c.saved_bytes(), 9);
    }

    #[test]
    fn saved_bytes_never_underflows() {
        let c = Compressed::projected("hi", ContentType::Prose, 999);
        assert_eq!(c.saved_bytes(), 0);
    }
}
