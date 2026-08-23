use crate::{SourceFileId, SourceSpan, TextRange};
use std::fmt;

/// One syntax-only error discovered while lexing or parsing a source file.
///
/// Compiler-facing diagnostic compatibility is deliberately handled by the
/// later syntax-to-semantic conversion boundary rather than by sharing types.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntaxDiagnostic {
    code: &'static str,
    message: String,
    span: SourceSpan,
    labels: Vec<SyntaxDiagnosticLabel>,
}

impl SyntaxDiagnostic {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>, span: SourceSpan) -> Self {
        Self {
            code,
            message: message.into(),
            span,
            labels: Vec::new(),
        }
    }

    /// Adds a label validated against the diagnostic's source-file ID.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxDiagnosticError::LabelSourceMismatch`] when the label
    /// was created for a different source-file ID.
    pub fn with_label(
        mut self,
        span: SourceSpan,
        message: impl Into<String>,
    ) -> Result<Self, SyntaxDiagnosticError> {
        if span.source() != self.span.source() {
            return Err(SyntaxDiagnosticError::LabelSourceMismatch {
                diagnostic_source: self.span.source(),
                label_source: span.source(),
            });
        }
        self.labels.push(SyntaxDiagnosticLabel {
            range: span.range(),
            message: message.into(),
        });
        Ok(self)
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub const fn source(&self) -> SourceFileId {
        self.span.source()
    }

    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.span.range()
    }

    #[must_use]
    pub fn labels(&self) -> &[SyntaxDiagnosticLabel] {
        &self.labels
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntaxDiagnosticError {
    LabelSourceMismatch {
        diagnostic_source: SourceFileId,
        label_source: SourceFileId,
    },
}

impl fmt::Display for SyntaxDiagnosticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LabelSourceMismatch {
                diagnostic_source,
                label_source,
            } => write!(
                formatter,
                "syntax diagnostic source {} does not match label source {}",
                diagnostic_source.get(),
                label_source.get()
            ),
        }
    }
}

impl std::error::Error for SyntaxDiagnosticError {}

/// Additional syntax range attached to a [`SyntaxDiagnostic`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntaxDiagnosticLabel {
    range: TextRange,
    message: String,
}

impl SyntaxDiagnosticLabel {
    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.range
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceFile;

    #[test]
    fn syntax_diagnostics_remain_distinct_from_compiler_diagnostics() {
        let source = SourceFile::new(SourceFileId::new(4), "abc").unwrap();
        let span = source.span(1, 3).unwrap();
        let label = source.span(3, 3).unwrap();
        let diagnostic = SyntaxDiagnostic::new("S0001", "expected expression", span)
            .with_label(label, "insert an expression here")
            .unwrap();

        assert_eq!(diagnostic.code(), "S0001");
        assert_eq!(diagnostic.message(), "expected expression");
        assert_eq!(diagnostic.source(), SourceFileId::new(4));
        assert_eq!(diagnostic.range(), span.range());
        assert_eq!(diagnostic.labels()[0].range(), label.range());
        assert_eq!(
            diagnostic.labels()[0].message(),
            "insert an expression here"
        );
    }

    #[test]
    fn rejects_labels_from_another_source() {
        let first = SourceFile::new(SourceFileId::new(1), "a").unwrap();
        let second = SourceFile::new(SourceFileId::new(2), "b").unwrap();
        let diagnostic = SyntaxDiagnostic::new("S0001", "error", first.span(0, 1).unwrap());

        assert_eq!(
            diagnostic.with_label(second.span(0, 1).unwrap(), "other source"),
            Err(SyntaxDiagnosticError::LabelSourceMismatch {
                diagnostic_source: SourceFileId::new(1),
                label_source: SourceFileId::new(2),
            })
        );
    }
}
