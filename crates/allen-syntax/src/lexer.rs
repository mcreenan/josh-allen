//! Lossless, context-neutral lexical analysis for ALLEN source text.

use crate::{SourceFile, SourceFileId, SyntaxDiagnostic, SyntaxKind, TextRange};

/// Frozen resource limits shared by syntax producers and consumers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxLimits {
    pub source_bytes: usize,
    pub tokens: usize,
    pub lexer_mode_depth: usize,
    pub interpolation_brace_depth: usize,
    pub parser_recursion: usize,
    pub delimiter_depth: usize,
    pub events: usize,
    pub nodes: usize,
    pub diagnostics: usize,
}

impl SyntaxLimits {
    /// Phase 22's frozen syntax-infrastructure defaults.
    ///
    /// These are not production language policy until the differential and
    /// canonical-cutover chunks adopt this frontend.
    pub const DEFAULT: Self = Self {
        source_bytes: 64 * 1024 * 1024,
        tokens: 1_048_576,
        lexer_mode_depth: 256,
        interpolation_brace_depth: 256,
        parser_recursion: 256,
        delimiter_depth: 256,
        events: 5_242_880,
        nodes: 2_097_152,
        diagnostics: 1_024,
    };
}

impl Default for SyntaxLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One source-backed lexical token. EOF is the sole zero-width token.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LexToken {
    source: SourceFileId,
    kind: SyntaxKind,
    range: TextRange,
}

impl LexToken {
    #[must_use]
    pub const fn kind(self) -> SyntaxKind {
        self.kind
    }

    #[must_use]
    pub const fn range(self) -> TextRange {
        self.range
    }

    /// Returns this token's exact, undecoded slice of `source`.
    ///
    /// # Panics
    ///
    /// Panics when called with a source file other than the one this token was
    /// produced from.
    #[must_use]
    pub fn text(self, source: &SourceFile) -> &str {
        assert_eq!(
            self.source,
            source.id(),
            "LexToken used with a different SourceFile"
        );
        source
            .text_at(self.range)
            .expect("LexToken ranges are validated against their SourceFile")
    }
}

/// The complete lossless lexical result for a [`SourceFile`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lexed {
    tokens: Vec<LexToken>,
    diagnostics: Vec<SyntaxDiagnostic>,
}

impl Lexed {
    #[must_use]
    pub fn tokens(&self) -> &[LexToken] {
        &self.tokens
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[SyntaxDiagnostic] {
        &self.diagnostics
    }

    /// Reconstructs the source exactly, excluding the zero-width EOF token.
    #[must_use]
    pub fn round_trip(&self, source: &SourceFile) -> String {
        self.tokens
            .iter()
            .filter(|token| token.kind != SyntaxKind::Eof)
            .map(|token| token.text(source))
            .collect()
    }
}

/// Lexes `source` with the frozen default syntax limits.
#[must_use]
pub fn lex(source: &SourceFile) -> Lexed {
    lex_with_limits(source, SyntaxLimits::DEFAULT)
}

/// Lexes `source` with explicit limits, primarily for boundedness tests.
#[must_use]
pub fn lex_with_limits(source: &SourceFile, limits: SyntaxLimits) -> Lexed {
    Lexer::new(source, limits).run()
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Normal,
    TemplateSegment { opener: usize },
    Interpolation { opener: usize, brace_depth: usize },
}

struct Lexer<'a> {
    source: &'a SourceFile,
    text: &'a str,
    limits: SyntaxLimits,
    offset: usize,
    tokens: Vec<LexToken>,
    diagnostics: Vec<SyntaxDiagnostic>,
    modes: Vec<Mode>,
    terminated: bool,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a SourceFile, limits: SyntaxLimits) -> Self {
        Self {
            source,
            text: source.text(),
            limits,
            offset: 0,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
            modes: vec![Mode::Normal],
            terminated: false,
        }
    }

    fn run(mut self) -> Lexed {
        if self.text.len() > self.limits.source_bytes {
            self.limit_at(0);
        }

        while !self.terminated && self.offset < self.text.len() {
            match self.modes.last().copied().unwrap_or(Mode::Normal) {
                Mode::Normal | Mode::Interpolation { .. } => self.lex_normal(),
                Mode::TemplateSegment { .. } => self.lex_template_segment(),
            }
        }

        if !self.terminated {
            match self.modes.last().copied().unwrap_or(Mode::Normal) {
                Mode::TemplateSegment { opener } => {
                    self.diagnostic("S0004", "unterminated template literal", opener, opener + 1);
                }
                Mode::Interpolation { opener, .. } => {
                    self.diagnostic(
                        "S0004",
                        "unterminated template interpolation",
                        opener,
                        opener + 2,
                    );
                }
                Mode::Normal => {}
            }
        }

        self.push_eof();
        Lexed {
            tokens: self.tokens,
            diagnostics: self.diagnostics,
        }
    }

    fn lex_normal(&mut self) {
        let start = self.offset;
        let rest = &self.text[start..];

        if rest.starts_with(' ') || rest.starts_with('\t') {
            self.offset += rest
                .bytes()
                .take_while(|byte| *byte == b' ' || *byte == b'\t')
                .count();
            self.token(SyntaxKind::Whitespace, start, self.offset);
            return;
        }
        if rest.starts_with("\r\n") {
            self.offset += 2;
            self.token(SyntaxKind::Newline, start, self.offset);
            return;
        }
        if rest.starts_with('\r') || rest.starts_with('\n') {
            self.offset += 1;
            self.token(SyntaxKind::Newline, start, self.offset);
            return;
        }
        if rest.starts_with("//") {
            self.lex_line_comment();
            return;
        }
        if rest.starts_with("/*") {
            self.lex_block_comment();
            return;
        }
        if rest.starts_with("b\"") {
            self.lex_quoted(start, true);
            return;
        }
        if rest.starts_with('"') {
            self.lex_quoted(start, false);
            return;
        }

        if self.in_interpolation() && rest.starts_with('}') {
            self.offset += 1;
            self.close_interpolation_or_brace(start);
            return;
        }
        if rest.starts_with('{') {
            self.offset += 1;
            if self.increment_interpolation_brace(start) {
                self.token(SyntaxKind::LBrace, start, self.offset);
            }
            return;
        }
        if rest.starts_with('`') {
            self.offset += 1;
            if self.push_mode(Mode::TemplateSegment { opener: start }, start) {
                self.token(SyntaxKind::Backtick, start, self.offset);
            }
            return;
        }
        if rest.as_bytes()[0].is_ascii_digit() {
            self.lex_number();
            return;
        }
        if is_ident_start(rest.as_bytes()[0]) {
            self.lex_identifier();
            return;
        }

        for (spelling, kind) in OPERATORS {
            if rest.starts_with(spelling) {
                self.offset += spelling.len();
                self.token(*kind, start, self.offset);
                return;
            }
        }

        let scalar_len = rest.chars().next().map_or(1, char::len_utf8);
        self.offset += scalar_len;
        self.error("S0001", "unexpected character", start, self.offset);
    }

    fn lex_template_segment(&mut self) {
        let start = self.offset;
        let rest = &self.text[start..];
        if rest.starts_with('`') {
            self.offset += 1;
            self.modes.pop();
            self.token(SyntaxKind::Backtick, start, self.offset);
            return;
        }
        if rest.starts_with("${") {
            self.offset += 2;
            if self.push_mode(
                Mode::Interpolation {
                    opener: start,
                    brace_depth: 0,
                },
                start,
            ) {
                self.token(SyntaxKind::TemplateExprStart, start, self.offset);
            }
            return;
        }
        if rest.starts_with('\\') {
            let escape_len = template_escape_len(rest);
            if let Some(escape_len) = escape_len {
                self.offset += escape_len;
                self.token(SyntaxKind::TemplateEscape, start, self.offset);
            } else {
                self.offset += invalid_escape_len(rest);
                self.error("S0002", "malformed template escape", start, self.offset);
            }
            return;
        }

        let character = rest.chars().next().expect("template mode has source text");
        if character.is_control() {
            self.offset += character.len_utf8();
            self.error(
                "S0002",
                "unescaped control character in template",
                start,
                self.offset,
            );
            return;
        }

        let mut end = start;
        while end < self.text.len() {
            let segment = &self.text[end..];
            if segment.starts_with('`') || segment.starts_with("${") || segment.starts_with('\\') {
                break;
            }
            let character = segment.chars().next().expect("in-bounds UTF-8 character");
            if character.is_control() {
                break;
            }
            end += character.len_utf8();
        }
        self.offset = end;
        self.token(SyntaxKind::TemplateTextScalar, start, end);
    }

    fn lex_line_comment(&mut self) {
        let start = self.offset;
        self.offset += 2;
        while self.offset < self.text.len() {
            let byte = self.text.as_bytes()[self.offset];
            if byte == b'\r' || byte == b'\n' {
                break;
            }
            self.offset += self.text[self.offset..]
                .chars()
                .next()
                .expect("in-bounds UTF-8 character")
                .len_utf8();
        }
        self.token(SyntaxKind::LineComment, start, self.offset);
    }

    fn lex_block_comment(&mut self) {
        let start = self.offset;
        self.offset += 2;
        let mut depth = 1usize;
        let mut openers = vec![start];
        let mut overflow = None;
        while self.offset < self.text.len() {
            let rest = &self.text[self.offset..];
            if rest.starts_with("/*") {
                depth = depth.saturating_add(1);
                if depth <= 128 {
                    openers.push(self.offset);
                } else if overflow.is_none() {
                    overflow = Some(self.offset);
                }
                self.offset += 2;
            } else if rest.starts_with("*/") {
                depth = depth.saturating_sub(1);
                self.offset += 2;
                if depth < 128 {
                    openers.pop();
                }
                if depth == 0 {
                    break;
                }
            } else {
                self.offset += rest
                    .chars()
                    .next()
                    .expect("in-bounds UTF-8 character")
                    .len_utf8();
            }
        }

        if let Some(opener) = overflow {
            if !self.diagnostic(
                "S0005",
                "block comment nesting exceeds 128",
                opener,
                opener + 2,
            ) {
                self.limit_at(start);
                return;
            }
            self.token(SyntaxKind::ErrorToken, start, self.offset);
        } else {
            if depth != 0 {
                let opener = openers.last().copied().unwrap_or(start);
                if !self.diagnostic("S0006", "unterminated block comment", opener, opener + 2) {
                    self.limit_at(start);
                    return;
                }
            }
            self.token(
                if depth == 0 {
                    SyntaxKind::BlockComment
                } else {
                    SyntaxKind::ErrorToken
                },
                start,
                self.offset,
            );
        }
    }

    fn lex_quoted(&mut self, start: usize, bytes: bool) {
        self.offset += if bytes { 2 } else { 1 };
        let mut valid = true;
        let mut closed = false;
        while self.offset < self.text.len() {
            let rest = &self.text[self.offset..];
            if rest.starts_with('"') {
                self.offset += 1;
                closed = true;
                break;
            }
            let character = rest.chars().next().expect("in-bounds UTF-8 character");
            if character.is_control() {
                valid = false;
                break;
            }
            if character == '\\' {
                let length = if bytes {
                    bytes_escape_len(rest)
                } else {
                    ordinary_escape_len(rest)
                };
                if let Some(length) = length {
                    self.offset += length;
                } else {
                    valid = false;
                    self.offset += invalid_escape_len(rest);
                }
                continue;
            }
            if bytes && !character.is_ascii() {
                valid = false;
            }
            self.offset += character.len_utf8();
        }

        if valid && closed {
            self.token(
                if bytes {
                    SyntaxKind::BytesLiteral
                } else {
                    SyntaxKind::StringLiteral
                },
                start,
                self.offset,
            );
        } else {
            self.error("S0002", "malformed literal", start, self.offset);
        }
    }

    fn lex_number(&mut self) {
        let start = self.offset;
        self.consume_digits();
        let mut float = false;
        let mut malformed = false;
        if self.text[self.offset..].starts_with('.')
            && self.text.as_bytes().get(self.offset + 1) != Some(&b'.')
        {
            float = true;
            self.offset += 1;
            let fraction_start = self.offset;
            self.consume_digits();
            malformed = fraction_start == self.offset;
            if matches!(self.text.as_bytes().get(self.offset), Some(b'e' | b'E')) {
                self.offset += 1;
                if matches!(self.text.as_bytes().get(self.offset), Some(b'+' | b'-')) {
                    self.offset += 1;
                }
                let exponent_start = self.offset;
                self.consume_digits();
                malformed |= exponent_start == self.offset;
            }
        }
        if malformed {
            self.error("S0003", "malformed number", start, self.offset);
        } else if float {
            self.token(SyntaxKind::FloatLiteral, start, self.offset);
        } else {
            self.token(SyntaxKind::IntLiteral, start, self.offset);
        }
    }

    fn consume_digits(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.offset)
            .is_some_and(u8::is_ascii_digit)
        {
            self.offset += 1;
        }
    }

    fn lex_identifier(&mut self) {
        let start = self.offset;
        self.offset += 1;
        while self
            .text
            .as_bytes()
            .get(self.offset)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            self.offset += 1;
        }
        let kind = keyword_kind(&self.text[start..self.offset]).unwrap_or(SyntaxKind::Ident);
        self.token(kind, start, self.offset);
    }

    fn in_interpolation(&self) -> bool {
        matches!(self.modes.last(), Some(Mode::Interpolation { .. }))
    }

    fn increment_interpolation_brace(&mut self, start: usize) -> bool {
        let Some(Mode::Interpolation { brace_depth, .. }) = self.modes.last_mut() else {
            return true;
        };
        if *brace_depth >= self.limits.interpolation_brace_depth {
            self.limit_at(start);
            return false;
        }
        *brace_depth += 1;
        true
    }

    fn close_interpolation_or_brace(&mut self, start: usize) {
        let Some(Mode::Interpolation { brace_depth, .. }) = self.modes.last_mut() else {
            self.token(SyntaxKind::RBrace, start, self.offset);
            return;
        };
        if *brace_depth == 0 {
            self.modes.pop();
        } else {
            *brace_depth -= 1;
        }
        self.token(SyntaxKind::RBrace, start, self.offset);
    }

    fn push_mode(&mut self, mode: Mode, start: usize) -> bool {
        if self.modes.len() >= self.limits.lexer_mode_depth {
            self.limit_at(start);
            return false;
        }
        self.modes.push(mode);
        true
    }

    fn token(&mut self, kind: SyntaxKind, start: usize, end: usize) {
        if self.terminated {
            return;
        }
        if self.token_would_exceed_source_capacity(end) {
            self.limit_at(start);
            return;
        }
        let range = self
            .source
            .range(start, end)
            .expect("lexer computes UTF-8 ranges");
        self.tokens.push(LexToken {
            source: self.source.id(),
            kind,
            range,
        });
    }

    fn token_would_exceed_source_capacity(&self, end: usize) -> bool {
        let source_capacity = self.source_token_capacity();
        let used_source_slots = self.tokens.len();
        let no_source_slots_remain = used_source_slots >= source_capacity;
        let source_suffix_remains = end < self.text.len();
        let would_use_reserved_suffix_slot = used_source_slots >= source_capacity.saturating_sub(1);

        no_source_slots_remain || (source_suffix_remains && would_use_reserved_suffix_slot)
    }

    fn source_token_capacity(&self) -> usize {
        // EOF consumes one token slot. Limits below two are raised to the
        // smallest lossless representation: one source token plus EOF.
        self.limits.tokens.max(2) - 1
    }

    fn error(&mut self, code: &'static str, message: &'static str, start: usize, end: usize) {
        if !self.diagnostic(code, message, start, end) {
            self.limit_at(start);
            return;
        }
        self.token(SyntaxKind::ErrorToken, start, end);
    }

    fn diagnostic(
        &mut self,
        code: &'static str,
        message: &'static str,
        start: usize,
        end: usize,
    ) -> bool {
        if self.diagnostics.len() >= self.limits.diagnostics {
            return false;
        }
        let span = self
            .source
            .span(start, end)
            .expect("lexer computes UTF-8 spans");
        self.diagnostics
            .push(SyntaxDiagnostic::new(code, message, span));
        true
    }

    fn limit_at(&mut self, start: usize) {
        if self.terminated {
            return;
        }
        let end = self.text.len();
        if start < end {
            if self.diagnostics.len() < self.limits.diagnostics {
                let span = self
                    .source
                    .span(start, end)
                    .expect("lexer computes UTF-8 spans");
                self.diagnostics.push(SyntaxDiagnostic::new(
                    "S0007",
                    "syntax lexer limit exceeded",
                    span,
                ));
            }
            let range = self
                .source
                .range(start, end)
                .expect("lexer computes UTF-8 ranges");
            debug_assert!(self.tokens.len() < self.source_token_capacity());
            self.tokens.push(LexToken {
                source: self.source.id(),
                kind: SyntaxKind::ErrorToken,
                range,
            });
        }
        self.offset = end;
        self.terminated = true;
    }

    fn push_eof(&mut self) {
        let range = self
            .source
            .range(self.text.len(), self.text.len())
            .expect("source end is a UTF-8 boundary");
        debug_assert!(self.tokens.len() < self.limits.tokens.max(2));
        self.tokens.push(LexToken {
            source: self.source.id(),
            kind: SyntaxKind::Eof,
            range,
        });
    }
}

const OPERATORS: &[(&str, SyntaxKind)] = &[
    ("+=", SyntaxKind::PlusEq),
    ("-=", SyntaxKind::MinusEq),
    ("*=", SyntaxKind::StarEq),
    ("/=", SyntaxKind::SlashEq),
    ("%=", SyntaxKind::PercentEq),
    ("..", SyntaxKind::DotDot),
    ("||", SyntaxKind::PipePipe),
    ("&&", SyntaxKind::AmpAmp),
    ("==", SyntaxKind::EqEq),
    ("!=", SyntaxKind::NotEq),
    ("<=", SyntaxKind::LtEq),
    (">=", SyntaxKind::GtEq),
    ("=>", SyntaxKind::FatArrow),
    ("@", SyntaxKind::At),
    ("{", SyntaxKind::LBrace),
    ("}", SyntaxKind::RBrace),
    ("[", SyntaxKind::LBracket),
    ("]", SyntaxKind::RBracket),
    ("(", SyntaxKind::LParen),
    (")", SyntaxKind::RParen),
    (",", SyntaxKind::Comma),
    (":", SyntaxKind::Colon),
    (";", SyntaxKind::Semi),
    ("<", SyntaxKind::Lt),
    (">", SyntaxKind::Gt),
    ("=", SyntaxKind::Eq),
    ("+", SyntaxKind::Plus),
    ("-", SyntaxKind::Minus),
    ("*", SyntaxKind::Star),
    ("/", SyntaxKind::Slash),
    ("%", SyntaxKind::Percent),
    ("!", SyntaxKind::Bang),
    (".", SyntaxKind::Dot),
    ("?", SyntaxKind::Question),
];

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn keyword_kind(text: &str) -> Option<SyntaxKind> {
    Some(match text {
        "manifest" => SyntaxKind::KwManifest,
        "import" => SyntaxKind::KwImport,
        "from" => SyntaxKind::KwFrom,
        "as" => SyntaxKind::KwAs,
        "export" => SyntaxKind::KwExport,
        "record" => SyntaxKind::KwRecord,
        "enum" => SyntaxKind::KwEnum,
        "type" => SyntaxKind::KwType,
        "async" => SyntaxKind::KwAsync,
        "fn" => SyntaxKind::KwFn,
        "returns" => SyntaxKind::KwReturns,
        "effects" => SyntaxKind::KwEffects,
        "let" => SyntaxKind::KwLet,
        "mut" => SyntaxKind::KwMut,
        "return" => SyntaxKind::KwReturn,
        "break" => SyntaxKind::KwBreak,
        "continue" => SyntaxKind::KwContinue,
        "while" => SyntaxKind::KwWhile,
        "loop" => SyntaxKind::KwLoop,
        "for" => SyntaxKind::KwFor,
        "in" => SyntaxKind::KwIn,
        "await" => SyntaxKind::KwAwait,
        "spawn" => SyntaxKind::KwSpawn,
        "true" => SyntaxKind::KwTrue,
        "false" => SyntaxKind::KwFalse,
        "map" => SyntaxKind::KwMap,
        "match" => SyntaxKind::KwMatch,
        "if" => SyntaxKind::KwIf,
        "else" => SyntaxKind::KwElse,
        "prompt" => SyntaxKind::KwPrompt,
        _ => return None,
    })
}

fn ordinary_escape_len(text: &str) -> Option<usize> {
    matches!(
        text.as_bytes().get(1),
        Some(b'"' | b'\\' | b'n' | b'r' | b't' | b'0' | b'b' | b'f')
    )
    .then_some(2)
}

fn bytes_escape_len(text: &str) -> Option<usize> {
    if let Some(length) = ordinary_escape_len(text) {
        return Some(length);
    }
    (text.as_bytes().get(1) == Some(&b'x')
        && text.as_bytes().get(2).is_some_and(u8::is_ascii_hexdigit)
        && text.as_bytes().get(3).is_some_and(u8::is_ascii_hexdigit))
    .then_some(4)
}

fn template_escape_len(text: &str) -> Option<usize> {
    ordinary_escape_len(text)
        .or_else(|| text.starts_with("\\`").then_some(2))
        .or_else(|| text.starts_with("\\${").then_some(3))
}

fn invalid_escape_len(text: &str) -> usize {
    text.get(1..)
        .and_then(|rest| rest.chars().next())
        .map_or(1, |character| 1 + character.len_utf8())
}

/// Accepts ordinary positive `Int` values plus the magnitude of `Int::MIN`.
/// The latter is valid only immediately after unary `-`; that contextual check
/// belongs to parsing/lowering rather than the context-neutral lexer.
pub(crate) fn int_magnitude_supported(text: &str) -> bool {
    let digits = text.trim_start_matches('0');
    digits.len() < 19 || (digits.len() == 19 && digits <= "9223372036854775808")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceFileId;

    fn source(text: &str) -> SourceFile {
        SourceFile::new(SourceFileId::new(1), text).unwrap()
    }

    fn kinds(text: &str) -> Vec<SyntaxKind> {
        let source = source(text);
        lex(&source)
            .tokens()
            .iter()
            .map(|token| token.kind())
            .collect()
    }

    #[test]
    fn round_trips_trivia_and_invalid_utf8_scalars_losslessly() {
        let source = source(" \t\n\r\n\r// x\r/* 🦀 */\u{000b}\u{000c}é");
        let lexed = lex(&source);
        assert_eq!(lexed.round_trip(&source), source.text());
        assert!(
            lexed
                .tokens()
                .iter()
                .all(|token| token.kind() == SyntaxKind::Eof || !token.range().is_empty())
        );
        assert_eq!(
            kinds("\n\r\n\r"),
            vec![
                SyntaxKind::Newline,
                SyntaxKind::Newline,
                SyntaxKind::Newline,
                SyntaxKind::Eof
            ]
        );
    }

    #[test]
    fn keeps_effect_id_components_context_neutral() {
        assert_eq!(
            kinds("tool.github.create_issue@2"),
            vec![
                SyntaxKind::Ident,
                SyntaxKind::Dot,
                SyntaxKind::Ident,
                SyntaxKind::Dot,
                SyntaxKind::Ident,
                SyntaxKind::At,
                SyntaxKind::IntLiteral,
                SyntaxKind::Eof
            ]
        );
    }

    #[test]
    #[should_panic(expected = "LexToken used with a different SourceFile")]
    fn token_text_rejects_an_equal_shaped_different_source() {
        let original = SourceFile::new(SourceFileId::new(1), "same").unwrap();
        let different = SourceFile::new(SourceFileId::new(2), "same").unwrap();
        let token = lex(&original).tokens()[0];
        let _ = token.text(&different);
    }

    #[test]
    fn number_boundaries_and_overflow_are_explicit() {
        assert_eq!(
            kinds("1..2 1.2e-3"),
            vec![
                SyntaxKind::IntLiteral,
                SyntaxKind::DotDot,
                SyntaxKind::IntLiteral,
                SyntaxKind::Whitespace,
                SyntaxKind::FloatLiteral,
                SyntaxKind::Eof
            ]
        );
        let source = source("1. 1.e2 1.2e 9223372036854775809");
        let lexed = lex(&source);
        assert_eq!(lexed.tokens()[0].kind(), SyntaxKind::ErrorToken);
        assert_eq!(lexed.tokens()[2].kind(), SyntaxKind::ErrorToken);
        assert_eq!(lexed.tokens()[4].kind(), SyntaxKind::ErrorToken);
        assert_eq!(lexed.tokens()[6].kind(), SyntaxKind::IntLiteral);
        assert_eq!(lexed.diagnostics().len(), 3);
        assert_eq!(lexed.round_trip(&source), source.text());
        assert_eq!(
            kinds("9223372036854775808"),
            vec![SyntaxKind::IntLiteral, SyntaxKind::Eof]
        );
        assert_eq!(
            kinds("9223372036854775809"),
            vec![SyntaxKind::IntLiteral, SyntaxKind::Eof]
        );
        assert!(!int_magnitude_supported("9223372036854775809"));
    }

    #[test]
    fn comments_nest_and_report_the_correct_openers() {
        let valid = format!("{}x{}", "/*".repeat(128), "*/".repeat(128));
        assert!(lex(&source(&valid)).diagnostics().is_empty());
        let too_deep = format!("{}{}", "/*".repeat(129), "*/".repeat(129));
        let lexed = lex(&source(&too_deep));
        assert_eq!(lexed.diagnostics().len(), 1);
        assert_eq!(lexed.diagnostics()[0].code(), "S0005");
        let unclosed = source("/* /*");
        let lexed = lex(&unclosed);
        assert_eq!(lexed.tokens()[0].kind(), SyntaxKind::ErrorToken);
        assert_eq!(
            lexed.diagnostics()[0].range(),
            unclosed.range(3, 5).unwrap()
        );
    }

    #[test]
    fn strings_bytes_and_nested_templates_are_lossless() {
        let source = source("\"\\\"\\\\\\n\\r\\t\\0\\b\\f🦀\" b\"\\x20\\n\" `a\\${${`b${x}`}}c`");
        let lexed = lex(&source);
        assert_eq!(lexed.round_trip(&source), source.text());
        assert!(lexed.diagnostics().is_empty());
        assert!(
            lexed
                .tokens()
                .iter()
                .any(|token| token.kind() == SyntaxKind::TemplateEscape)
        );
        assert!(
            lexed
                .tokens()
                .iter()
                .any(|token| token.kind() == SyntaxKind::TemplateExprStart)
        );
    }

    #[test]
    fn closing_template_does_not_skip_the_following_token() {
        assert_eq!(
            kinds("`x`foo+bar"),
            vec![
                SyntaxKind::Backtick,
                SyntaxKind::TemplateTextScalar,
                SyntaxKind::Backtick,
                SyntaxKind::Ident,
                SyntaxKind::Plus,
                SyntaxKind::Ident,
                SyntaxKind::Eof,
            ]
        );
    }

    #[test]
    fn only_normative_reserved_words_are_global_keywords() {
        assert_eq!(
            kinds("fn language(name: List) returns None { _ } type"),
            vec![
                SyntaxKind::KwFn,
                SyntaxKind::Whitespace,
                SyntaxKind::Ident,
                SyntaxKind::LParen,
                SyntaxKind::Ident,
                SyntaxKind::Colon,
                SyntaxKind::Whitespace,
                SyntaxKind::Ident,
                SyntaxKind::RParen,
                SyntaxKind::Whitespace,
                SyntaxKind::KwReturns,
                SyntaxKind::Whitespace,
                SyntaxKind::Ident,
                SyntaxKind::Whitespace,
                SyntaxKind::LBrace,
                SyntaxKind::Whitespace,
                SyntaxKind::Ident,
                SyntaxKind::Whitespace,
                SyntaxKind::RBrace,
                SyntaxKind::Whitespace,
                SyntaxKind::KwType,
                SyntaxKind::Eof,
            ]
        );
    }

    #[test]
    fn legacy_return_arrow_is_two_ordinary_operator_tokens() {
        assert_eq!(
            kinds("->"),
            vec![SyntaxKind::Minus, SyntaxKind::Gt, SyntaxKind::Eof]
        );
    }

    #[test]
    fn reports_unterminated_template_modes_at_eof() {
        for text in ["`text", "`${value"] {
            let source = source(text);
            let lexed = lex(&source);
            assert_eq!(lexed.round_trip(&source), text);
            assert_eq!(lexed.diagnostics()[0].code(), "S0004");
            assert_eq!(lexed.tokens().last().unwrap().kind(), SyntaxKind::Eof);
        }
    }

    #[test]
    fn injected_input_mode_brace_and_diagnostic_limits_terminate_losslessly() {
        let input = source("abcd");
        let lexed = lex_with_limits(
            &input,
            SyntaxLimits {
                source_bytes: 3,
                ..SyntaxLimits::DEFAULT
            },
        );
        assert_eq!(lexed.round_trip(&input), input.text());
        assert_eq!(lexed.tokens()[0].kind(), SyntaxKind::ErrorToken);

        let input = source("`${{x}}`");
        let lexed = lex_with_limits(
            &input,
            SyntaxLimits {
                interpolation_brace_depth: 0,
                ..SyntaxLimits::DEFAULT
            },
        );
        assert_eq!(lexed.round_trip(&input), input.text());
        assert!(
            lexed
                .tokens()
                .iter()
                .any(|token| token.kind() == SyntaxKind::ErrorToken)
        );

        let input = source("`x`");
        let lexed = lex_with_limits(
            &input,
            SyntaxLimits {
                lexer_mode_depth: 1,
                ..SyntaxLimits::DEFAULT
            },
        );
        assert_eq!(lexed.round_trip(&input), input.text());
        assert_eq!(lexed.tokens()[0].kind(), SyntaxKind::ErrorToken);

        let input = source("\u{000b}\u{000c}");
        let lexed = lex_with_limits(
            &input,
            SyntaxLimits {
                diagnostics: 1,
                ..SyntaxLimits::DEFAULT
            },
        );
        assert_eq!(lexed.round_trip(&input), input.text());
        assert_eq!(lexed.diagnostics().len(), 1);
        assert_eq!(lexed.tokens().last().unwrap().kind(), SyntaxKind::Eof);
    }

    #[test]
    fn recovery_kinds_have_their_intended_token_classification() {
        assert!(!SyntaxKind::Error.is_token());
        assert!(SyntaxKind::ErrorToken.is_token());
        assert!(!SyntaxKind::Missing.is_token());
        assert!(crate::LEXICAL_KIND_INVENTORY.contains(&"token:@"));
    }

    #[test]
    fn tiny_limits_coalesce_the_remaining_source() {
        for (text, token_limit, expect_error) in [
            ("a b c", 2, true),
            ("a ", 2, true),
            ("a b", 3, true),
            ("x", 1, false),
            ("x", 0, false),
        ] {
            let source = source(text);
            let limits = SyntaxLimits {
                tokens: token_limit,
                ..SyntaxLimits::DEFAULT
            };
            let lexed = lex_with_limits(&source, limits);
            assert_eq!(lexed.round_trip(&source), source.text());
            assert_eq!(lexed.tokens().last().unwrap().kind(), SyntaxKind::Eof);
            assert!(lexed.tokens().len() <= token_limit.max(2));
            assert_eq!(
                lexed
                    .tokens()
                    .iter()
                    .any(|token| token.kind() == SyntaxKind::ErrorToken),
                expect_error
            );
        }
    }
}
