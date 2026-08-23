//! Correctness-first bounded incremental reparsing.

use crate::{
    GreenToken, Parse, SourceFile, SourceFileId, SyntaxKind, SyntaxNode, TextRange, TextRangeError,
    TextSize, lex_with_limits, parse_with_limits,
};
use rowan::SyntaxKind as RowanKind;
use std::{fmt, sync::Arc};

/// One validated UTF-8 text replacement against an immutable source snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextEdit {
    source: SourceFile,
    range: TextRange,
    replacement: Arc<str>,
}

impl TextEdit {
    /// Creates an edit with half-open UTF-8 byte offsets in `source`.
    ///
    /// # Errors
    ///
    /// Returns a range error when either endpoint is invalid for `source`.
    pub fn new(
        source: &SourceFile,
        start: usize,
        end: usize,
        replacement: impl Into<Arc<str>>,
    ) -> Result<Self, TextRangeError> {
        Ok(Self {
            source: source.clone(),
            range: source.range(start, end)?,
            replacement: replacement.into(),
        })
    }

    #[must_use]
    pub const fn source_id(&self) -> SourceFileId {
        self.source.id()
    }

    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.range
    }

    #[must_use]
    pub fn replacement(&self) -> &str {
        &self.replacement
    }

    /// Applies this edit and returns the next immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for a different logical source, a stale same-ID source
    /// snapshot, or when the edited text cannot fit Rowan's 32-bit text range
    /// model.
    pub fn apply(&self, source: &SourceFile) -> Result<SourceFile, TextEditError> {
        if source.id() != self.source.id() {
            return Err(TextEditError::SourceMismatch {
                edit_source: self.source.id(),
                actual_source: source.id(),
            });
        }
        if !source.same_snapshot(&self.source) {
            return Err(TextEditError::StaleSourceSnapshot {
                source: source.id(),
            });
        }
        let start = text_size_to_usize(self.range.start());
        let end = text_size_to_usize(self.range.end());
        source.range(start, end)?;
        let new_len = source
            .text()
            .len()
            .checked_sub(end - start)
            .and_then(|length| length.checked_add(self.replacement.len()))
            .ok_or(TextEditError::ResultTooLarge)?;
        if u32::try_from(new_len).is_err() {
            return Err(TextEditError::ResultTooLarge);
        }
        let mut text = String::with_capacity(new_len);
        text.push_str(&source.text()[..start]);
        text.push_str(&self.replacement);
        text.push_str(&source.text()[end..]);
        SourceFile::from_string(source.id(), text).map_err(TextEditError::Range)
    }
}

/// Invalid use of an otherwise syntax-only text edit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextEditError {
    Range(TextRangeError),
    SourceMismatch {
        edit_source: SourceFileId,
        actual_source: SourceFileId,
    },
    StaleSourceSnapshot {
        source: SourceFileId,
    },
    ParseSourceMismatch {
        parse_source: SourceFileId,
        actual_source: SourceFileId,
    },
    ResultTooLarge,
}

impl From<TextRangeError> for TextEditError {
    fn from(error: TextRangeError) -> Self {
        Self::Range(error)
    }
}

impl fmt::Display for TextEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Range(error) => error.fmt(formatter),
            Self::SourceMismatch {
                edit_source,
                actual_source,
            } => write!(
                formatter,
                "text edit source {} does not match source {}",
                edit_source.get(),
                actual_source.get()
            ),
            Self::StaleSourceSnapshot { source } => write!(
                formatter,
                "text edit was created for an older snapshot of source {}",
                source.get()
            ),
            Self::ParseSourceMismatch {
                parse_source,
                actual_source,
            } => write!(
                formatter,
                "parse source {} does not match source {}",
                parse_source.get(),
                actual_source.get()
            ),
            Self::ResultTooLarge => {
                write!(formatter, "edited source exceeds the syntax range model")
            }
        }
    }
}

impl std::error::Error for TextEditError {}

/// The syntax entry point used for one reparse operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReparseEntryPoint {
    Source,
    Token(SyntaxKind),
}

/// Why an edit correctly selected the full-parse fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReparseFallback {
    PreviousParseHasErrors,
    PreviousTreeDoesNotMatchSource,
    SourceLimitMayChange,
    EditCrossesTokenBoundary,
    UnsupportedTokenKind,
    TokenKindMayChange,
}

/// Deterministic, host-independent work evidence for one edit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReparseStatistics {
    edited_range: TextRange,
    entry_point: ReparseEntryPoint,
    source_bytes_copied: usize,
    bytes_relexed: usize,
    old_nodes_replaced: usize,
    new_nodes_replaced: usize,
    source_snapshot_checks: usize,
    cached_error_checks: usize,
    positional_token_lookups: usize,
    token_lookup_path_nodes: usize,
    fallback: Option<ReparseFallback>,
}

impl ReparseStatistics {
    #[must_use]
    pub const fn edited_range(&self) -> TextRange {
        self.edited_range
    }

    #[must_use]
    pub const fn entry_point(&self) -> ReparseEntryPoint {
        self.entry_point
    }

    /// Returns the edited-source bytes copied to create the next immutable
    /// snapshot. This is separate from lexer and tree work.
    #[must_use]
    pub const fn source_bytes_copied(&self) -> usize {
        self.source_bytes_copied
    }

    /// Returns the UTF-8 byte width selected for lexing: the complete edited
    /// source on fallback or the complete edited token on the local path.
    #[must_use]
    pub const fn bytes_relexed(&self) -> usize {
        self.bytes_relexed
    }

    #[must_use]
    pub const fn old_nodes_replaced(&self) -> usize {
        self.old_nodes_replaced
    }

    #[must_use]
    pub const fn new_nodes_replaced(&self) -> usize {
        self.new_nodes_replaced
    }

    /// Returns the constant-time immutable-source snapshot identity checks.
    #[must_use]
    pub const fn source_snapshot_checks(&self) -> usize {
        self.source_snapshot_checks
    }

    /// Returns the constant-time cached parse-error state checks.
    #[must_use]
    pub const fn cached_error_checks(&self) -> usize {
        self.cached_error_checks
    }

    /// Returns the number of Rowan offset lookups used to locate the edit.
    #[must_use]
    pub const fn positional_token_lookups(&self) -> usize {
        self.positional_token_lookups
    }

    /// Returns the syntax-node depths traversed by the positional token lookup
    /// paths. No complete-tree search is performed on the local path.
    #[must_use]
    pub const fn token_lookup_path_nodes(&self) -> usize {
        self.token_lookup_path_nodes
    }

    #[must_use]
    pub const fn full_fallback(&self) -> bool {
        self.fallback.is_some()
    }

    #[must_use]
    pub const fn fallback(&self) -> Option<ReparseFallback> {
        self.fallback
    }
}

/// The edited source snapshot, equivalent parse, and deterministic work data.
#[derive(Clone, Debug)]
pub struct IncrementalParse {
    source: SourceFile,
    parse: Parse,
    statistics: ReparseStatistics,
}

impl IncrementalParse {
    #[must_use]
    pub const fn source(&self) -> &SourceFile {
        &self.source
    }

    #[must_use]
    pub const fn parse(&self) -> &Parse {
        &self.parse
    }

    #[must_use]
    pub const fn statistics(&self) -> &ReparseStatistics {
        &self.statistics
    }

    #[must_use]
    pub fn into_parts(self) -> (SourceFile, Parse, ReparseStatistics) {
        (self.source, self.parse, self.statistics)
    }
}

/// Applies one edit using a proven-safe token-local replacement or a full
/// parse fallback. Both paths are required to produce the fresh full parse.
///
/// # Errors
///
/// Returns an error only when the edit or prior parse belongs to a different
/// source identity, the edit belongs to a stale same-ID source snapshot, or the
/// edited text cannot fit the syntax range model.
pub fn reparse(
    previous: &Parse,
    source: &SourceFile,
    edit: &TextEdit,
) -> Result<IncrementalParse, TextEditError> {
    if previous.source_id() != source.id() {
        return Err(TextEditError::ParseSourceMismatch {
            parse_source: previous.source_id(),
            actual_source: source.id(),
        });
    }
    let edited_source = edit.apply(source)?;
    let old_root = previous.syntax();
    let mut work = ValidationWork {
        source_snapshot_checks: 2,
        ..ValidationWork::default()
    };

    if let Some(reason) =
        previous_fallback_reason(previous, source, edited_source.text().len(), &mut work)
    {
        return Ok(full_fallback(
            previous,
            edited_source,
            edit.range,
            reason,
            work,
        ));
    }

    let Some(token) = single_edited_token(&old_root, edit.range) else {
        return Ok(full_fallback(
            previous,
            edited_source,
            edit.range,
            ReparseFallback::EditCrossesTokenBoundary,
            ValidationWork {
                positional_token_lookups: 2,
                ..work
            },
        ));
    };
    work.positional_token_lookups = 2;
    work.token_lookup_path_nodes = replaced_node_count(&token) * 2;
    if !is_token_local_kind(token.kind()) {
        return Ok(full_fallback(
            previous,
            edited_source,
            edit.range,
            ReparseFallback::UnsupportedTokenKind,
            work,
        ));
    }

    let token_start = text_size_to_usize(token.text_range().start());
    let edit_start = text_size_to_usize(edit.range.start()) - token_start;
    let edit_end = text_size_to_usize(edit.range.end()) - token_start;
    let mut token_text = String::with_capacity(
        token.text().len() - (edit_end - edit_start) + edit.replacement.len(),
    );
    token_text.push_str(&token.text()[..edit_start]);
    token_text.push_str(&edit.replacement);
    token_text.push_str(&token.text()[edit_end..]);

    let fragment =
        SourceFile::from_string(source.id(), token_text.clone()).map_err(TextEditError::Range)?;
    let lexed = lex_with_limits(&fragment, previous.limits());
    let same_single_token = lexed.diagnostics().is_empty()
        && matches!(lexed.tokens(), [only, eof] if only.kind() == token.kind()
            && only.range() == TextRange::new(TextSize::from(0), fragment.len())
            && eof.kind() == SyntaxKind::Eof);
    if !same_single_token {
        return Ok(full_fallback(
            previous,
            edited_source,
            edit.range,
            ReparseFallback::TokenKindMayChange,
            work,
        ));
    }

    Ok(finish_local_reparse(
        &token,
        &token_text,
        edited_source,
        edit.range,
        previous.limits(),
        work,
    ))
}

fn previous_fallback_reason(
    previous: &Parse,
    source: &SourceFile,
    edited_len: usize,
    work: &mut ValidationWork,
) -> Option<ReparseFallback> {
    if previous.same_source_snapshot(source) {
        work.cached_error_checks = 1;
        if previous.has_errors() {
            Some(ReparseFallback::PreviousParseHasErrors)
        } else if edited_len > previous.limits().source_bytes {
            Some(ReparseFallback::SourceLimitMayChange)
        } else {
            None
        }
    } else {
        Some(ReparseFallback::PreviousTreeDoesNotMatchSource)
    }
}

fn finish_local_reparse(
    token: &crate::SyntaxToken,
    token_text: &str,
    source: SourceFile,
    edited_range: TextRange,
    limits: crate::SyntaxLimits,
    work: ValidationWork,
) -> IncrementalParse {
    let replaced_nodes = replaced_node_count(token);
    let token_kind = token.kind();
    let source_bytes_copied = source.text().len();
    let green = token.replace_with(GreenToken::new(RowanKind(token_kind as u16), token_text));
    let parse = Parse {
        source: source.clone(),
        green,
        diagnostics: Vec::new(),
        has_errors: false,
        limits,
    };
    IncrementalParse {
        source,
        parse,
        statistics: ReparseStatistics {
            edited_range,
            entry_point: ReparseEntryPoint::Token(token_kind),
            source_bytes_copied,
            bytes_relexed: token_text.len(),
            old_nodes_replaced: replaced_nodes,
            new_nodes_replaced: replaced_nodes,
            source_snapshot_checks: work.source_snapshot_checks,
            cached_error_checks: work.cached_error_checks,
            positional_token_lookups: work.positional_token_lookups,
            token_lookup_path_nodes: work.token_lookup_path_nodes,
            fallback: None,
        },
    }
}

fn single_edited_token(root: &SyntaxNode, range: TextRange) -> Option<crate::SyntaxToken> {
    let start = root.token_at_offset(range.start()).right_biased();
    let end = root.token_at_offset(range.end()).left_biased();
    let (Some(start), Some(end)) = (start, end) else {
        return None;
    };
    if start != end || start.kind() == SyntaxKind::Eof {
        return None;
    }
    let token = start;
    if range.is_empty()
        && (range.start() == token.text_range().start()
            || range.start() == token.text_range().end())
    {
        return None;
    }
    Some(token)
}

fn is_token_local_kind(kind: SyntaxKind) -> bool {
    // The parser intentionally reclassifies some `Ident` spellings by text
    // (`None`, built-in constructors, manifest fields, type heads, and more),
    // so an equal lexical kind is not sufficient proof for identifiers.
    //
    // Horizontal trivia and quoted literals are parser-text-insensitive.
    // Newlines are deliberately excluded because adjacent CR/LF tokens can
    // coalesce after an edit. Quoted literal delimiters keep their token
    // boundaries independent of adjacent normal-mode tokens.
    matches!(
        kind,
        SyntaxKind::Whitespace | SyntaxKind::StringLiteral | SyntaxKind::BytesLiteral
    )
}

fn replaced_node_count(token: &crate::SyntaxToken) -> usize {
    token
        .parent()
        .map_or(0, |parent| parent.ancestors().count())
}

fn full_fallback(
    previous: &Parse,
    source: SourceFile,
    edited_range: TextRange,
    reason: ReparseFallback,
    work: ValidationWork,
) -> IncrementalParse {
    let old_nodes = previous.syntax().descendants().count();
    let parse = parse_with_limits(&source, previous.limits());
    let new_nodes = parse.syntax().descendants().count();
    let bytes_relexed = source.text().len();
    let source_bytes_copied = source.text().len();
    IncrementalParse {
        source,
        parse,
        statistics: ReparseStatistics {
            edited_range,
            entry_point: ReparseEntryPoint::Source,
            source_bytes_copied,
            bytes_relexed,
            old_nodes_replaced: old_nodes,
            new_nodes_replaced: new_nodes,
            source_snapshot_checks: work.source_snapshot_checks,
            cached_error_checks: work.cached_error_checks,
            positional_token_lookups: work.positional_token_lookups,
            token_lookup_path_nodes: work.token_lookup_path_nodes,
            fallback: Some(reason),
        },
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ValidationWork {
    source_snapshot_checks: usize,
    cached_error_checks: usize,
    positional_token_lookups: usize,
    token_lookup_path_nodes: usize,
}

fn text_size_to_usize(size: TextSize) -> usize {
    u32::from(size) as usize
}
