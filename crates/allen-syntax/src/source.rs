use crate::{TextRange, TextSize};
use std::{fmt, sync::Arc};

#[derive(Debug)]
enum SourceText {
    Shared(Arc<str>),
    Owned(String),
}

impl SourceText {
    fn as_str(&self) -> &str {
        match self {
            Self::Shared(text) => text,
            Self::Owned(text) => text,
        }
    }
}

/// Stable identity for one source file within a syntax consumer.
///
/// The syntax layer treats this value as opaque. Callers own the mapping from
/// IDs to paths, package modules, editor buffers, or other external identities.
/// Within one consumer, an ID identifies one logical file. Each [`SourceFile`]
/// is an immutable text snapshot; incremental edits may create a later snapshot
/// with the same ID.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceFileId(u32);

impl SourceFileId {
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A source-qualified range validated against one [`SourceFile`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceSpan {
    source: SourceFileId,
    range: TextRange,
}

impl SourceSpan {
    #[must_use]
    pub const fn source(self) -> SourceFileId {
        self.source
    }

    #[must_use]
    pub const fn range(self) -> TextRange {
        self.range
    }
}

/// Immutable UTF-8 source text and its syntax-layer identity.
#[derive(Clone, Debug)]
pub struct SourceFile {
    id: SourceFileId,
    text: Arc<SourceText>,
    len: TextSize,
}

impl PartialEq for SourceFile {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.text() == other.text()
    }
}

impl Eq for SourceFile {}

impl SourceFile {
    /// Creates a source file when its byte length fits the syntax range model.
    ///
    /// # Errors
    ///
    /// Returns [`TextRangeError::OffsetTooLarge`] when the source is too large
    /// for concrete-tree byte offsets.
    pub fn new(id: SourceFileId, text: impl Into<Arc<str>>) -> Result<Self, TextRangeError> {
        Self::from_text(id, SourceText::Shared(text.into()))
    }

    /// Creates a source file by moving an owned string without copying its
    /// source bytes.
    ///
    /// # Errors
    ///
    /// Returns [`TextRangeError::OffsetTooLarge`] when the source is too large
    /// for concrete-tree byte offsets.
    pub fn from_string(id: SourceFileId, text: String) -> Result<Self, TextRangeError> {
        Self::from_text(id, SourceText::Owned(text))
    }

    fn from_text(id: SourceFileId, text: SourceText) -> Result<Self, TextRangeError> {
        let text = Arc::new(text);
        let len = u32::try_from(text.as_str().len())
            .map(TextSize::from)
            .map_err(|_| TextRangeError::OffsetTooLarge {
                offset: text.as_str().len(),
            })?;
        Ok(Self { id, text, len })
    }

    #[must_use]
    pub const fn id(&self) -> SourceFileId {
        self.id
    }

    #[must_use]
    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    pub(crate) fn same_snapshot(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.text, &other.text)
    }

    #[must_use]
    pub const fn len(&self) -> TextSize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text().is_empty()
    }

    /// Validates an ordered, in-bounds range whose endpoints are UTF-8 byte
    /// boundaries in this file.
    ///
    /// # Errors
    ///
    /// Returns a [`TextRangeError`] when the endpoints are reversed, outside
    /// the source, or not UTF-8 boundaries.
    pub fn range(&self, start: usize, end: usize) -> Result<TextRange, TextRangeError> {
        let source_len = u32::from(self.len) as usize;

        if start > end {
            return Err(TextRangeError::Reversed { start, end });
        }
        if end > source_len {
            return Err(TextRangeError::OutOfBounds { end, source_len });
        }
        if !self.text().is_char_boundary(start) {
            return Err(TextRangeError::NotCharBoundary { offset: start });
        }
        if !self.text().is_char_boundary(end) {
            return Err(TextRangeError::NotCharBoundary { offset: end });
        }

        let start = u32::try_from(start)
            .map(TextSize::from)
            .map_err(|_| TextRangeError::OffsetTooLarge { offset: start })?;
        let end = u32::try_from(end)
            .map(TextSize::from)
            .map_err(|_| TextRangeError::OffsetTooLarge { offset: end })?;
        Ok(TextRange::new(start, end))
    }

    /// Creates a source-qualified span after validating the byte endpoints.
    ///
    /// # Errors
    ///
    /// Returns a [`TextRangeError`] when the endpoints are reversed, outside
    /// the source, or not UTF-8 boundaries.
    pub fn span(&self, start: usize, end: usize) -> Result<SourceSpan, TextRangeError> {
        self.range(start, end).map(|range| SourceSpan {
            source: self.id,
            range,
        })
    }

    /// Returns the exact original text covered by a previously validated
    /// range, or `None` when the range is not valid for this source.
    #[must_use]
    pub fn text_at(&self, range: TextRange) -> Option<&str> {
        self.text()
            .get(u32::from(range.start()) as usize..u32::from(range.end()) as usize)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextRangeError {
    OffsetTooLarge { offset: usize },
    Reversed { start: usize, end: usize },
    OutOfBounds { end: usize, source_len: usize },
    NotCharBoundary { offset: usize },
}

impl fmt::Display for TextRangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OffsetTooLarge { offset } => {
                write!(
                    formatter,
                    "UTF-8 byte offset {offset} exceeds the syntax limit"
                )
            }
            Self::Reversed { start, end } => {
                write!(
                    formatter,
                    "UTF-8 byte range starts at {start} after end {end}"
                )
            }
            Self::OutOfBounds { end, source_len } => write!(
                formatter,
                "UTF-8 byte range ends at {end} beyond source length {source_len}"
            ),
            Self::NotCharBoundary { offset } => {
                write!(
                    formatter,
                    "UTF-8 byte offset {offset} is not a character boundary"
                )
            }
        }
    }
}

impl std::error::Error for TextRangeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_utf8_ranges_and_preserves_original_text() {
        let source = SourceFile::new(SourceFileId::new(7), "a🦀z").unwrap();
        let crab = source.range(1, 5).unwrap();
        let span = source.span(1, 5).unwrap();

        assert_eq!(source.id().get(), 7);
        assert_eq!(u32::from(crab.len()), 4);
        assert_eq!(source.text_at(crab), Some("🦀"));
        assert_eq!(u32::from(source.len()), 6);
        assert_eq!(span.source(), SourceFileId::new(7));
        assert_eq!(span.range(), crab);
    }

    #[test]
    fn rejects_invalid_ranges() {
        let source = SourceFile::new(SourceFileId::new(0), "a🦀z").unwrap();

        assert_eq!(
            source.range(2, 5),
            Err(TextRangeError::NotCharBoundary { offset: 2 })
        );
        assert_eq!(
            source.range(5, 7),
            Err(TextRangeError::OutOfBounds {
                end: 7,
                source_len: 6,
            })
        );
        assert_eq!(
            source.range(5, 1),
            Err(TextRangeError::Reversed { start: 5, end: 1 })
        );
    }

    #[test]
    fn rejects_offsets_that_cannot_fit_the_tree_model() {
        assert!(u32::try_from(usize::MAX).is_err());
    }
}
