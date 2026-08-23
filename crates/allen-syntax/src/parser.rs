//! Bounded, lossless parser for the syntax-only frontend.

use crate::{
    GreenNode, LexToken, SourceFile, SourceFileId, SyntaxDiagnostic, SyntaxKind, SyntaxLimits,
    SyntaxNode, lex_with_limits, tree_sink,
};
use rowan::{GreenNodeBuilder, SyntaxKind as RowanKind};

mod body;
mod expressions;
mod manifest;
mod patterns;
mod types;

/// A concrete syntax tree and its ordered lexical/parser diagnostics.
#[derive(Clone, Debug)]
pub struct Parse {
    pub(crate) source: SourceFile,
    pub(crate) green: GreenNode,
    pub(crate) diagnostics: Vec<SyntaxDiagnostic>,
    pub(crate) has_errors: bool,
    pub(crate) limits: SyntaxLimits,
}

impl Parse {
    /// Returns the immutable source identity from which this tree was parsed.
    #[must_use]
    pub const fn source_id(&self) -> SourceFileId {
        self.source.id()
    }

    /// Returns the resource limits used to build this tree.
    #[must_use]
    pub const fn limits(&self) -> SyntaxLimits {
        self.limits
    }

    #[must_use]
    pub const fn green(&self) -> &GreenNode {
        &self.green
    }

    #[must_use]
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[SyntaxDiagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub const fn has_errors(&self) -> bool {
        self.has_errors
    }

    /// Asserts that the tree's tokens reproduce the original source byte-for-byte.
    ///
    /// # Panics
    ///
    /// Panics when the tree does not exactly reproduce `source`.
    pub fn assert_round_trip(&self, source: &SourceFile) {
        assert!(
            self.tree_matches_source(source),
            "parse tree is not lossless for the complete source"
        );
    }

    pub(crate) fn same_source_snapshot(&self, source: &SourceFile) -> bool {
        self.source.same_snapshot(source)
    }

    fn tree_matches_source(&self, source: &SourceFile) -> bool {
        let mut offset = 0usize;
        for token in self
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .filter(|token| token.kind() != SyntaxKind::Eof)
        {
            let Some(end) = offset.checked_add(token.text().len()) else {
                return false;
            };
            if source.text().get(offset..end) != Some(token.text()) {
                return false;
            }
            offset = end;
        }
        offset == source.text().len()
    }
}

/// Parses with the frozen default syntax resource limits.
#[must_use]
pub fn parse(source: &SourceFile) -> Parse {
    parse_with_limits(source, SyntaxLimits::DEFAULT)
}

/// Parses with explicit resource limits, returning a lossless fallback tree on
/// any parser/event/node bound rather than exposing partial structure.
#[must_use]
pub fn parse_with_limits(source: &SourceFile, limits: SyntaxLimits) -> Parse {
    let limits = SyntaxLimits {
        nodes: limits.nodes.max(1),
        ..limits
    };
    let lexed = lex_with_limits(source, limits);
    let mut diagnostics: Vec<_> = lexed
        .diagnostics()
        .iter()
        .take(limits.diagnostics)
        .cloned()
        .collect();
    let mut parser = Parser::new(source, lexed.tokens(), limits, &mut diagnostics);
    parser.source();
    if parser.exceeded {
        push_limit_diagnostic(source, &mut diagnostics, limits);
        return fallback(source, &diagnostics, limits);
    }
    let events = parser.events;
    let green = match tree_sink::sink(events, source, lexed.tokens()) {
        Ok(green) if SyntaxNode::new_root(green.clone()).kind() == SyntaxKind::Source => green,
        _ => {
            push_limit_diagnostic(source, &mut diagnostics, limits);
            return fallback(source, &diagnostics, limits);
        }
    };
    let has_errors = !diagnostics.is_empty() || green_has_errors(&green);
    let parsed = Parse {
        source: source.clone(),
        green,
        diagnostics,
        has_errors,
        limits,
    };
    parsed.assert_round_trip(source);
    parsed
}

fn fallback(source: &SourceFile, diagnostics: &[SyntaxDiagnostic], limits: SyntaxLimits) -> Parse {
    let mut builder = GreenNodeBuilder::new();
    builder.start_node(RowanKind(SyntaxKind::Source as u16));
    if limits.nodes >= 2 {
        builder.start_node(RowanKind(SyntaxKind::Error as u16));
        if !source.is_empty() {
            builder.token(RowanKind(SyntaxKind::ErrorToken as u16), source.text());
        }
        builder.finish_node();
    } else if !source.is_empty() {
        builder.token(RowanKind(SyntaxKind::ErrorToken as u16), source.text());
    }
    builder.token(RowanKind(SyntaxKind::Eof as u16), "");
    builder.finish_node();
    Parse {
        source: source.clone(),
        green: builder.finish(),
        diagnostics: diagnostics.to_vec(),
        has_errors: true,
        limits,
    }
}

fn green_has_errors(green: &GreenNode) -> bool {
    SyntaxNode::new_root(green.clone())
        .descendants_with_tokens()
        .any(|element| match element {
            rowan::NodeOrToken::Node(node) => {
                matches!(node.kind(), SyntaxKind::Error | SyntaxKind::Missing)
            }
            rowan::NodeOrToken::Token(token) => token.kind() == SyntaxKind::ErrorToken,
        })
}

fn push_limit_diagnostic(
    source: &SourceFile,
    diagnostics: &mut Vec<SyntaxDiagnostic>,
    limits: SyntaxLimits,
) {
    if diagnostics.len() < limits.diagnostics {
        let span = source
            .span(source.text().len(), source.text().len())
            .expect("source end span");
        diagnostics.push(SyntaxDiagnostic::new(
            "S0100",
            "syntax parser limit exceeded",
            span,
        ));
    }
}

#[derive(Clone, Copy, Debug)]
struct Marker {
    pos: usize,
}

#[derive(Clone, Copy, Debug)]
struct CompletedMarker {
    pos: usize,
}

struct Parser<'a, 'd> {
    source: &'a SourceFile,
    tokens: &'a [LexToken],
    limits: SyntaxLimits,
    diagnostics: &'d mut Vec<SyntaxDiagnostic>,
    events: Vec<tree_sink::Event>,
    pos: usize,
    exceeded: bool,
    recursion: usize,
    expression_depth: usize,
    postfix_brace_boundary: Option<usize>,
    delimiter_depth: usize,
    node_count: usize,
}

impl<'a, 'd> Parser<'a, 'd> {
    fn new(
        source: &'a SourceFile,
        tokens: &'a [LexToken],
        limits: SyntaxLimits,
        diagnostics: &'d mut Vec<SyntaxDiagnostic>,
    ) -> Self {
        Self {
            source,
            tokens,
            limits,
            diagnostics,
            events: Vec::new(),
            pos: 0,
            exceeded: false,
            recursion: 0,
            expression_depth: 0,
            postfix_brace_boundary: None,
            delimiter_depth: 0,
            node_count: 0,
        }
    }

    fn source(&mut self) {
        let marker = self.start();
        let mut manifest_allowed = true;
        let mut imports_allowed = true;
        while !self.exceeded {
            self.eat_trivia();
            match self.nth(0) {
                SyntaxKind::Eof => {
                    self.bump_nontrivia();
                    break;
                }
                SyntaxKind::KwManifest if manifest_allowed => {
                    manifest_allowed = false;
                    self.inline_manifest();
                }
                SyntaxKind::KwImport if imports_allowed => {
                    manifest_allowed = false;
                    self.import_declaration();
                }
                SyntaxKind::KwManifest | SyntaxKind::KwImport => self.error_until_top_level(),
                SyntaxKind::KwRecord
                | SyntaxKind::KwEnum
                | SyntaxKind::KwType
                | SyntaxKind::KwFn
                | SyntaxKind::KwAsync
                | SyntaxKind::KwExport => {
                    manifest_allowed = false;
                    imports_allowed = false;
                    if self.is_declaration_start() {
                        self.declaration();
                    } else {
                        self.error_until_top_level();
                    }
                }
                _ => {
                    manifest_allowed = false;
                    self.error_until_top_level();
                }
            }
        }
        self.complete(marker, SyntaxKind::Source);
    }

    fn import_declaration(&mut self) {
        let marker = self.start();
        self.bump_nontrivia();
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` after `import`") {
            self.complete(marker, SyntaxKind::ImportDeclaration);
            return;
        }
        let mut need_name = true;
        let mut saw_name = false;
        while !self.exceeded && !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            if need_name {
                let name = self.start();
                self.expect(SyntaxKind::Ident, "expected imported name");
                if self.at(SyntaxKind::KwAs) {
                    self.bump_nontrivia();
                    self.expect(SyntaxKind::Ident, "expected alias after `as`");
                }
                self.complete(name, SyntaxKind::ImportName);
                need_name = false;
                saw_name = true;
            }
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
                need_name = true;
            } else if !self.at(SyntaxKind::RBrace) {
                self.error_one("expected `,` or `}` in import list");
            }
        }
        if !saw_name {
            self.missing("expected imported name");
        }
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after import list");
        self.expect(SyntaxKind::KwFrom, "expected `from` after import list");
        self.expect(SyntaxKind::StringLiteral, "expected import source string");
        self.expect(SyntaxKind::Semi, "expected `;` after import declaration");
        self.complete(marker, SyntaxKind::ImportDeclaration);
    }

    fn declaration(&mut self) {
        let marker = self.start();
        let offset = usize::from(self.at(SyntaxKind::KwExport));
        match self.nth(offset) {
            SyntaxKind::KwRecord => self.record_declaration(),
            SyntaxKind::KwEnum => self.enum_declaration(),
            SyntaxKind::KwType => self.type_alias_declaration(),
            SyntaxKind::KwAsync | SyntaxKind::KwFn => self.function_declaration(),
            _ => self.error_until_top_level(),
        }
        self.complete(marker, SyntaxKind::Declaration);
    }

    fn error_until_top_level(&mut self) {
        let marker = self.start();
        let mut consumed = false;
        while !self.exceeded && !self.at(SyntaxKind::Eof) {
            if consumed && self.is_top_level_sync() {
                break;
            }
            self.bump();
            consumed = true;
        }
        if !consumed {
            self.missing("expected top-level declaration");
        }
        self.complete(marker, SyntaxKind::Error);
        self.error("unexpected top-level syntax");
    }

    fn is_declaration_start(&self) -> bool {
        let mut offset = 0;
        if self.nth(offset) == SyntaxKind::KwExport {
            offset += 1;
        }
        matches!(
            self.nth(offset),
            SyntaxKind::KwRecord
                | SyntaxKind::KwEnum
                | SyntaxKind::KwType
                | SyntaxKind::KwFn
                | SyntaxKind::KwAsync
        )
    }

    fn is_top_level_sync(&self) -> bool {
        matches!(
            self.nth(0),
            SyntaxKind::KwImport
                | SyntaxKind::KwManifest
                | SyntaxKind::KwExport
                | SyntaxKind::KwRecord
                | SyntaxKind::KwEnum
                | SyntaxKind::KwType
                | SyntaxKind::KwAsync
                | SyntaxKind::KwFn
        )
    }

    fn expect(&mut self, kind: SyntaxKind, message: &'static str) -> bool {
        self.eat_trivia();
        if self.current_kind() == kind {
            self.bump_nontrivia();
            true
        } else {
            self.missing(message);
            false
        }
    }

    fn expect_open_delimiter(&mut self, kind: SyntaxKind, message: &'static str) -> bool {
        self.expect(kind, message) && self.enter_delimiter()
    }

    fn expect_close_delimiter(&mut self, kind: SyntaxKind, message: &'static str) {
        self.expect(kind, message);
        self.exit_delimiter();
    }

    fn error_one(&mut self, message: &'static str) {
        self.eat_trivia();
        if self.at(SyntaxKind::Eof) {
            self.missing(message);
        } else {
            let marker = self.start();
            self.bump_nontrivia();
            self.complete(marker, SyntaxKind::Error);
        }
        self.error(message);
    }

    fn missing(&mut self, message: &'static str) {
        let marker = self.start();
        self.complete(marker, SyntaxKind::Missing);
        self.error(message);
    }

    fn error(&mut self, message: &'static str) {
        if self.diagnostics.len() >= self.limits.diagnostics {
            self.exceed();
            return;
        }
        let offset = self.current_offset();
        let span = self
            .source
            .span(offset, offset)
            .expect("token offsets derive from source");
        self.diagnostics
            .push(SyntaxDiagnostic::new("S0101", message, span));
    }

    fn eat_trivia(&mut self) {
        while !self.exceeded && is_trivia(self.current_kind()) {
            self.bump();
        }
    }

    /// Emits intervening trivia in the current node, then consumes the token
    /// selected by grammar lookahead.
    fn bump_nontrivia(&mut self) {
        self.eat_trivia();
        self.bump();
    }

    fn at(&self, kind: SyntaxKind) -> bool {
        self.nth(0) == kind
    }

    fn nth(&self, mut nontrivia: usize) -> SyntaxKind {
        let mut index = self.pos;
        while let Some(token) = self.tokens.get(index) {
            if !is_trivia(token.kind()) {
                if nontrivia == 0 {
                    return token.kind();
                }
                nontrivia -= 1;
            }
            index += 1;
        }
        SyntaxKind::Eof
    }

    fn nth_text(&self, mut nontrivia: usize) -> Option<&'a str> {
        let mut index = self.pos;
        while let Some(token) = self.tokens.get(index) {
            if !is_trivia(token.kind()) {
                if nontrivia == 0 {
                    return Some(token.text(self.source));
                }
                nontrivia -= 1;
            }
            index += 1;
        }
        None
    }

    fn current_kind(&self) -> SyntaxKind {
        self.tokens
            .get(self.pos)
            .map_or(SyntaxKind::Eof, |token| token.kind())
    }

    fn current_offset(&self) -> usize {
        self.tokens.get(self.pos).map_or_else(
            || self.source.text().len(),
            |token| u32::from(token.range().start()) as usize,
        )
    }

    fn bump(&mut self) {
        if self.exceeded || self.pos >= self.tokens.len() {
            return;
        }
        if self.current_kind() == SyntaxKind::ErrorToken {
            let marker = self.start();
            self.token();
            self.complete(marker, SyntaxKind::Error);
        } else {
            self.token();
        }
    }

    fn token(&mut self) {
        self.token_as(1, None);
    }

    fn token_as(&mut self, token_count: usize, override_kind: Option<SyntaxKind>) {
        let Ok(token_index) = u32::try_from(self.pos) else {
            self.exceed();
            return;
        };
        let Ok(token_count_u32) = u32::try_from(token_count) else {
            self.exceed();
            return;
        };
        let Some(next_pos) = self.pos.checked_add(token_count) else {
            self.exceed();
            return;
        };
        if token_count == 0 || next_pos > self.tokens.len() {
            self.exceed();
            return;
        }
        self.push(tree_sink::Event::Token {
            token_index,
            token_count: token_count_u32,
            override_kind,
        });
        self.pos = next_pos;
    }

    fn start(&mut self) -> Marker {
        if self.recursion >= self.limits.parser_recursion {
            self.exceed();
            return Marker {
                pos: self.events.len(),
            };
        }
        self.recursion += 1;
        let pos = self.events.len();
        self.push(tree_sink::Event::Tombstone);
        Marker { pos }
    }

    fn complete(&mut self, marker: Marker, kind: SyntaxKind) -> CompletedMarker {
        if let Some(event) = self.events.get_mut(marker.pos) {
            *event = tree_sink::Event::Start {
                kind,
                forward_parent: None,
            };
        }
        self.push(tree_sink::Event::Finish);
        self.recursion = self.recursion.saturating_sub(1);
        CompletedMarker { pos: marker.pos }
    }

    #[allow(dead_code)]
    fn precede(&mut self, completed: CompletedMarker) -> Marker {
        let marker = self.start();
        let Some(distance) = marker
            .pos
            .checked_sub(completed.pos)
            .and_then(|value| u32::try_from(value).ok())
        else {
            self.exceed();
            return marker;
        };
        match self.events.get_mut(completed.pos) {
            Some(tree_sink::Event::Start { forward_parent, .. }) => {
                *forward_parent = Some(distance);
            }
            _ => self.exceed(),
        }
        marker
    }

    fn push(&mut self, event: tree_sink::Event) {
        if self.exceeded {
            return;
        }
        let is_node = matches!(event, tree_sink::Event::Tombstone);
        if self.events.len() >= self.limits.events
            || (is_node && self.node_count >= self.limits.nodes)
        {
            self.exceed();
        } else {
            if is_node {
                let Some(next) = self.node_count.checked_add(1) else {
                    self.exceed();
                    return;
                };
                self.node_count = next;
            }
            self.events.push(event);
        }
    }

    fn exceed(&mut self) {
        self.exceeded = true;
        self.events.clear();
    }

    fn enter_delimiter(&mut self) -> bool {
        if self.exceeded {
            return false;
        }
        if self.delimiter_depth >= self.limits.delimiter_depth {
            self.exceed();
            false
        } else {
            self.delimiter_depth += 1;
            true
        }
    }

    fn exit_delimiter(&mut self) {
        self.delimiter_depth = self.delimiter_depth.saturating_sub(1);
    }
}

fn is_trivia(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Whitespace
            | SyntaxKind::Newline
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Source, SourceFileId};
    use rowan::ast::AstNode;

    fn source(text: &str) -> SourceFile {
        SourceFile::new(SourceFileId::new(1), text).unwrap()
    }
    fn kinds(parse: &Parse) -> Vec<SyntaxKind> {
        parse
            .syntax()
            .descendants()
            .map(|node| node.kind())
            .collect()
    }

    #[test]
    fn source_and_minimal_function_are_lossless() {
        for text in ["", " // only trivia\n", "fn f() returns Void {}"] {
            let source = source(text);
            let parsed = parse(&source);
            assert_eq!(parsed.source_id(), source.id());
            assert!(Source::cast(parsed.syntax()).is_some());
            parsed.assert_round_trip(&source);
        }
    }

    #[test]
    fn top_level_kinds_and_contextual_words_are_deterministic() {
        let source = source(
            "import { a as b, } from \"m\"; record R { x: T } enum E { A } fn f() returns T {}",
        );
        let parsed = parse(&source);
        let kinds = kinds(&parsed);
        for kind in [
            SyntaxKind::ImportDeclaration,
            SyntaxKind::ImportName,
            SyntaxKind::RecordDeclaration,
            SyntaxKind::EnumDeclaration,
            SyntaxKind::FunctionDeclaration,
            SyntaxKind::Body,
        ] {
            assert!(kinds.contains(&kind), "missing {kind:?}");
        }
        assert!(parsed.diagnostics().is_empty());
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn missing_syntax_recovers_to_later_declaration() {
        let source = source("import { } from \"m\" record Later { x: T }");
        let parsed = parse(&source);
        assert!(kinds(&parsed).contains(&SyntaxKind::Missing));
        assert!(kinds(&parsed).contains(&SyntaxKind::RecordDeclaration));
        assert!(!parsed.diagnostics().is_empty());
        assert!(parsed.has_errors());
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn source_enforces_manifest_import_declaration_order_losslessly() {
        let text = "manifest { language: \"0.1\" } import { first } from \"a\"; fn first() returns Void {} manifest { language: \"0.1\" } import { late } from \"b\"; record Later { value: T } fn final() returns Void {}";
        let file = source(text);
        let parsed = parse(&file);
        assert!(parsed.has_errors());
        parsed.assert_round_trip(&file);

        let root = parsed.syntax();
        assert_eq!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::InlineManifest)
                .count(),
            1,
            "a late or repeated manifest must not become a second manifest node"
        );
        assert_eq!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::ImportDeclaration)
                .count(),
            1,
            "an import after a declaration must not become an import node"
        );
        assert_eq!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                .count(),
            2
        );
        assert_eq!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::RecordDeclaration)
                .count(),
            1,
            "top-level ordering recovery swallowed the later declaration"
        );

        let repeated = source(
            "manifest { language: \"0.1\" } manifest { entry: main } fn main() returns Void {}",
        );
        let parsed = parse(&repeated);
        assert!(parsed.has_errors());
        parsed.assert_round_trip(&repeated);
        assert_eq!(
            parsed
                .syntax()
                .descendants()
                .filter(|node| node.kind() == SyntaxKind::InlineManifest)
                .count(),
            1
        );
    }

    #[test]
    fn lexer_errors_are_preserved_once_and_first() {
        let source = source("# fn ok() returns T {}");
        let parsed = parse(&source);
        let errors = parsed
            .syntax()
            .descendants()
            .filter(|node| node.kind() == SyntaxKind::Error)
            .count();
        let error_tokens = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .filter(|token| token.kind() == SyntaxKind::ErrorToken)
            .count();
        assert!(errors >= 1);
        assert_eq!(error_tokens, 1);
        assert_eq!(parsed.diagnostics()[0].code(), "S0001");
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn unexpected_closers_and_limits_are_lossless() {
        let source = source("} fn ok() returns T {}");
        parse(&source).assert_round_trip(&source);
        for limits in [
            SyntaxLimits {
                events: 1,
                ..SyntaxLimits::DEFAULT
            },
            SyntaxLimits {
                nodes: 1,
                ..SyntaxLimits::DEFAULT
            },
            SyntaxLimits {
                delimiter_depth: 1,
                ..SyntaxLimits::DEFAULT
            },
            SyntaxLimits {
                parser_recursion: 0,
                ..SyntaxLimits::DEFAULT
            },
            SyntaxLimits {
                diagnostics: 0,
                ..SyntaxLimits::DEFAULT
            },
        ] {
            let parsed = parse_with_limits(&source, limits);
            assert_eq!(parsed.source_id(), source.id());
            assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
            assert!(parsed.has_errors());
            parsed.assert_round_trip(&source);
        }
    }

    #[test]
    fn injected_node_limits_obey_the_source_root_floor() {
        let file = source("fn f() returns Void {}");
        for requested in 0..=2 {
            let parsed = parse_with_limits(
                &file,
                SyntaxLimits {
                    nodes: requested,
                    ..SyntaxLimits::DEFAULT
                },
            );
            assert_eq!(parsed.source_id(), file.id());
            parsed.assert_round_trip(&file);
            assert!(parsed.has_errors(), "node limit {requested} was ignored");

            let root = parsed.syntax();
            let node_count = root.descendants().count();
            assert!(
                node_count <= requested.max(1),
                "node limit {requested} produced {node_count} nodes"
            );
            assert_eq!(
                root.descendants()
                    .filter(|node| node.kind() == SyntaxKind::Error)
                    .count(),
                usize::from(requested >= 2),
                "Error must not exceed the effective node cap"
            );
            assert!(
                root.descendants_with_tokens()
                    .filter_map(rowan::NodeOrToken::into_token)
                    .any(|token| token.kind() == SyntaxKind::ErrorToken)
            );
        }

        let empty = source("");
        let parsed = parse_with_limits(
            &empty,
            SyntaxLimits {
                nodes: 0,
                ..SyntaxLimits::DEFAULT
            },
        );
        assert_eq!(parsed.source_id(), empty.id());
        assert!(!parsed.has_errors());
        assert_eq!(parsed.syntax().descendants().count(), 1);
        parsed.assert_round_trip(&empty);
    }

    #[test]
    fn spaced_tokens_and_mixed_delimiters_remain_valid_and_lossless() {
        let source = source(
            "import { a as b } from \"m\"; fn f ( x : Map < A , List < B > > ) returns T { ([x]) }",
        );
        let parsed = parse(&source);
        assert!(parsed.diagnostics().is_empty());
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn marker_precede_builds_forward_parent() {
        let source = source("x");
        let tokens = lex_with_limits(&source, SyntaxLimits::DEFAULT);
        let mut diagnostics = Vec::new();
        let mut parser = Parser::new(
            &source,
            tokens.tokens(),
            SyntaxLimits::DEFAULT,
            &mut diagnostics,
        );
        let inner = parser.start();
        parser.bump();
        parser.bump();
        let inner = parser.complete(inner, SyntaxKind::Primary);
        let outer = parser.precede(inner);
        parser.complete(outer, SyntaxKind::Expression);
        let green = tree_sink::sink(parser.events, &source, tokens.tokens()).unwrap();
        assert_eq!(SyntaxNode::new_root(green).kind(), SyntaxKind::Expression);
    }

    #[test]
    fn valid_manifest_and_exact_syntax_forms_do_not_create_errors() {
        let source = source(
            "manifest { language: \"0.1\" } record R { item: Map < A , List < B > > } enum E { V ( Map < A , B > ) } fn f ( x : T ) returns T { ( [ x ] ) }",
        );
        let parsed = parse(&source);
        assert!(parsed.diagnostics().is_empty());
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn template_interpolation_closer_does_not_close_the_function_body() {
        let source = source("fn f() returns String { return `value ${1 + {x: 2}.x}`; }");
        let parsed = parse(&source);
        assert!(parsed.diagnostics().is_empty());
        assert!(!parsed.has_errors());
        assert_eq!(
            kinds(&parsed)
                .iter()
                .filter(|kind| **kind == SyntaxKind::FunctionDeclaration)
                .count(),
            1
        );
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn tree_sink_rejects_non_monotonic_or_incomplete_tokens() {
        let source = source("x");
        let lexed = lex_with_limits(&source, SyntaxLimits::DEFAULT);
        let events = vec![
            tree_sink::Event::Start {
                kind: SyntaxKind::Source,
                forward_parent: None,
            },
            tree_sink::Event::Token {
                token_index: 1,
                token_count: 1,
                override_kind: None,
            },
            tree_sink::Event::Finish,
        ];
        assert!(tree_sink::sink(events, &source, lexed.tokens()).is_err());
    }

    #[test]
    fn contextual_compound_token_keeps_exact_source_text() {
        let source = source("tool.github.create_issue@2");
        let lexed = lex_with_limits(&source, SyntaxLimits::DEFAULT);
        let mut diagnostics = Vec::new();
        let mut parser = Parser::new(
            &source,
            lexed.tokens(),
            SyntaxLimits::DEFAULT,
            &mut diagnostics,
        );
        let root = parser.start();
        parser.token_as(7, Some(SyntaxKind::EffectId));
        parser.bump();
        parser.complete(root, SyntaxKind::Source);
        let syntax =
            SyntaxNode::new_root(tree_sink::sink(parser.events, &source, lexed.tokens()).unwrap());
        let tokens: Vec<_> = syntax
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .collect();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].kind(), SyntaxKind::EffectId);
        assert_eq!(tokens[0].text(), source.text());
        assert_eq!(tokens[1].kind(), SyntaxKind::Eof);
    }
}
