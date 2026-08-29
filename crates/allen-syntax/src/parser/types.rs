use super::{Parser, is_trivia};
use crate::SyntaxKind;

const UNARY_GENERIC_TYPES: &[(&str, SyntaxKind)] = &[
    ("List", SyntaxKind::KwList),
    ("Option", SyntaxKind::KwOption),
    ("Future", SyntaxKind::KwFuture),
    ("Task", SyntaxKind::KwTask),
    ("Prompt", SyntaxKind::KwPromptType),
    ("Range", SyntaxKind::KwRange),
    ("Sequence", SyntaxKind::KwSequence),
];

const BINARY_GENERIC_TYPES: &[(&str, SyntaxKind)] = &[
    ("Map", SyntaxKind::KwMapType),
    ("Result", SyntaxKind::KwResult),
];

impl Parser<'_, '_> {
    pub(super) fn record_declaration(&mut self) {
        let marker = self.start();
        if self.at(SyntaxKind::KwExport) {
            self.bump_nontrivia();
        }
        self.expect(SyntaxKind::KwRecord, "expected `record`");
        self.expect(SyntaxKind::Ident, "expected record name");
        if self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` after record name") {
            self.record_fields();
            self.expect_close_delimiter(
                SyntaxKind::RBrace,
                "expected `}` after record declaration",
            );
        }
        if self.at(SyntaxKind::KwWhere) {
            self.bump_nontrivia();
            if self.expect_open_delimiter(
                SyntaxKind::LBrace,
                "expected `{` after record invariant `where`",
            ) {
                self.expression();
                self.expect_close_delimiter(
                    SyntaxKind::RBrace,
                    "expected `}` after record invariant",
                );
            }
        }
        self.complete(marker, SyntaxKind::RecordDeclaration);
    }

    pub(super) fn enum_declaration(&mut self) {
        let marker = self.start();
        if self.at(SyntaxKind::KwExport) {
            self.bump_nontrivia();
        }
        self.expect(SyntaxKind::KwEnum, "expected `enum`");
        self.expect(SyntaxKind::Ident, "expected enum name");
        if self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` after enum name") {
            let mut saw_variant = false;
            while !self.exceeded && !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
                if self.at(SyntaxKind::Ident) {
                    self.enum_variant();
                    saw_variant = true;
                } else {
                    self.error_one("expected enum variant");
                }
                if self.at(SyntaxKind::Comma) {
                    self.bump_nontrivia();
                } else if !self.at(SyntaxKind::Ident)
                    && !self.at(SyntaxKind::RBrace)
                    && !self.at(SyntaxKind::Eof)
                {
                    self.error_one("expected `,` or another enum variant");
                }
            }
            if !saw_variant {
                self.missing("expected enum variant");
            }
            self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after enum declaration");
        }
        self.complete(marker, SyntaxKind::EnumDeclaration);
    }

    pub(super) fn type_alias_declaration(&mut self) {
        let marker = self.start();
        if self.at(SyntaxKind::KwExport) {
            self.bump_nontrivia();
        }
        self.expect(SyntaxKind::KwType, "expected `type`");
        self.expect(SyntaxKind::Ident, "expected type alias name");
        self.expect(SyntaxKind::Eq, "expected `=` after type alias name");
        self.type_syntax();
        self.complete(marker, SyntaxKind::TypeAliasDeclaration);
    }

    pub(super) fn newtype_declaration(&mut self) {
        let marker = self.start();
        if self.at(SyntaxKind::KwExport) {
            self.bump_nontrivia();
        }
        self.expect(SyntaxKind::KwNewtype, "expected `newtype`");
        self.expect(SyntaxKind::Ident, "expected newtype name");
        self.expect(SyntaxKind::Eq, "expected `=` after newtype name");
        self.type_syntax();
        self.complete(marker, SyntaxKind::NewtypeDeclaration);
    }

    pub(super) fn const_declaration(&mut self) {
        let marker = self.start();
        if self.at(SyntaxKind::KwExport) {
            self.bump_nontrivia();
        }
        self.expect(SyntaxKind::KwConst, "expected `const`");
        self.expect(SyntaxKind::Ident, "expected constant name");
        self.expect(SyntaxKind::Colon, "expected `:` after constant name");
        self.type_syntax();
        self.expect(SyntaxKind::Eq, "expected `=` after constant type");
        self.expression();
        self.expect(SyntaxKind::Semi, "expected `;` after constant declaration");
        self.complete(marker, SyntaxKind::ConstDeclaration);
    }

    pub(super) fn function_declaration(&mut self) {
        let marker = self.start();
        if self.at(SyntaxKind::KwExport) {
            self.bump_nontrivia();
        }
        if self.at(SyntaxKind::KwAsync) {
            self.bump_nontrivia();
        }
        self.expect(SyntaxKind::KwFn, "expected `fn`");
        self.expect(SyntaxKind::Ident, "expected function name");
        if self.at(SyntaxKind::Lt) {
            self.generic_parameters();
        }
        self.parameter_list();
        self.expect(
            SyntaxKind::KwReturns,
            "expected `returns` before return type",
        );
        self.type_syntax();
        if self.at(SyntaxKind::KwEffects) {
            self.effect_clause();
        }
        if self.at(SyntaxKind::LBrace) {
            self.body();
        } else {
            self.missing("expected function body");
        }
        self.complete(marker, SyntaxKind::FunctionDeclaration);
    }

    pub(super) fn test_declaration(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::KwTest, "expected `test`");
        self.expect(SyntaxKind::StringLiteral, "expected test name string");
        if self.at(SyntaxKind::KwEffects) {
            self.effect_clause();
        }
        if self.at(SyntaxKind::LBrace) {
            self.body();
        } else {
            self.missing("expected test body");
        }
        self.complete(marker, SyntaxKind::TestDeclaration);
    }

    fn record_fields(&mut self) {
        while !self.exceeded && !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            if self.at(SyntaxKind::Ident) {
                self.record_field();
            } else {
                self.error_one("expected record field");
            }
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
            } else if !self.at(SyntaxKind::Ident)
                && !self.at(SyntaxKind::RBrace)
                && !self.at(SyntaxKind::Eof)
            {
                self.error_one("expected `,` or another record field");
            }
        }
    }

    fn record_field(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::Ident, "expected record field name");
        self.expect(SyntaxKind::Colon, "expected `:` after record field name");
        self.type_syntax();
        self.complete(marker, SyntaxKind::RecordField);
    }

    fn enum_variant(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::Ident, "expected enum variant name");
        if self.at(SyntaxKind::LParen) {
            self.expect_open_delimiter(SyntaxKind::LParen, "expected `(` before enum payload");
            if self.at(SyntaxKind::RParen) {
                self.missing("expected enum payload type");
            } else {
                self.type_syntax();
                while self.at(SyntaxKind::Comma) {
                    self.bump_nontrivia();
                    if self.at(SyntaxKind::RParen) {
                        break;
                    }
                    self.type_syntax();
                }
            }
            self.expect_close_delimiter(SyntaxKind::RParen, "expected `)` after enum payload");
        } else if self.at(SyntaxKind::LBrace) {
            self.expect_open_delimiter(
                SyntaxKind::LBrace,
                "expected `{` before enum record payload",
            );
            self.record_fields();
            self.expect_close_delimiter(
                SyntaxKind::RBrace,
                "expected `}` after enum record payload",
            );
        }
        self.complete(marker, SyntaxKind::EnumVariant);
    }

    fn generic_parameters(&mut self) {
        let marker = self.start();
        if !self.expect_open_delimiter(SyntaxKind::Lt, "expected `<` before generic parameters") {
            self.complete(marker, SyntaxKind::GenericParameters);
            return;
        }
        if self.at(SyntaxKind::Gt) {
            self.missing("expected generic parameter");
        } else {
            self.generic_parameter();
            while self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
                if self.at(SyntaxKind::Gt) {
                    break;
                }
                self.generic_parameter();
            }
        }
        self.expect_close_delimiter(SyntaxKind::Gt, "expected `>` after generic parameters");
        self.complete(marker, SyntaxKind::GenericParameters);
    }

    fn generic_parameter(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::Ident, "expected generic parameter name");
        self.expect(
            SyntaxKind::Colon,
            "expected `:` after generic parameter name",
        );
        self.expect_contextual_type("Eq", SyntaxKind::KwEq, "expected `Eq` generic bound");
        self.complete(marker, SyntaxKind::GenericParameter);
    }

    pub(super) fn parameter_list(&mut self) {
        if !self.expect_open_delimiter(SyntaxKind::LParen, "expected function parameter list") {
            return;
        }
        let mut expect_parameter = !self.at(SyntaxKind::RParen);
        while !self.exceeded
            && !matches!(
                self.nth(0),
                SyntaxKind::RParen
                    | SyntaxKind::RBracket
                    | SyntaxKind::Gt
                    | SyntaxKind::KwReturns
                    | SyntaxKind::LBrace
                    | SyntaxKind::RBrace
                    | SyntaxKind::Eof
            )
        {
            if self.at(SyntaxKind::Comma) {
                if expect_parameter {
                    self.error_one("expected parameter before `,`");
                } else {
                    self.bump_nontrivia();
                    expect_parameter = !self.at(SyntaxKind::RParen);
                }
            } else if self.at(SyntaxKind::Ident) {
                if !expect_parameter {
                    self.missing("expected `,` between parameters");
                }
                self.parameter();
                expect_parameter = false;
            } else {
                self.error_one("expected parameter");
                expect_parameter = false;
            }
        }
        self.expect_close_delimiter(SyntaxKind::RParen, "expected `)` after function parameters");
    }

    fn parameter(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::Ident, "expected parameter name");
        self.expect(SyntaxKind::Colon, "expected `:` after parameter name");
        self.type_syntax();
        if self.at(SyntaxKind::Eq) {
            self.bump_nontrivia();
            self.expression();
        }
        self.complete(marker, SyntaxKind::Parameter);
    }

    pub(super) fn type_syntax(&mut self) -> bool {
        let marker = self.start();
        let parsed = if self.at_generic_type() {
            self.generic_type();
            true
        } else {
            match self.nth(0) {
                SyntaxKind::Ident => {
                    let named = self.start();
                    self.bump_nontrivia();
                    while self.at(SyntaxKind::Dot) {
                        self.bump_nontrivia();
                        self.expect(
                            SyntaxKind::Ident,
                            "expected qualified named type segment after `.`",
                        );
                    }
                    self.complete(named, SyntaxKind::NamedType);
                    true
                }
                SyntaxKind::LParen => {
                    self.tuple_type();
                    true
                }
                SyntaxKind::LBrace => {
                    self.record_type();
                    true
                }
                SyntaxKind::KwFn => {
                    self.function_type();
                    true
                }
                _ => {
                    self.missing("expected type");
                    false
                }
            }
        };
        self.complete(marker, SyntaxKind::Type);
        parsed
    }

    fn generic_type(&mut self) {
        let marker = self.start();
        let Some((spelling, kind, arity)) = self.generic_type_head() else {
            self.missing("expected generic type constructor");
            self.complete(marker, SyntaxKind::GenericType);
            return;
        };
        self.expect_contextual_type(spelling, kind, "expected generic type constructor");
        if !self.expect_open_delimiter(
            SyntaxKind::Lt,
            "expected `<` after generic type constructor",
        ) {
            self.complete(marker, SyntaxKind::GenericType);
            return;
        }
        self.type_syntax();
        if arity == 2 {
            self.expect(
                SyntaxKind::Comma,
                "expected `,` between generic type arguments",
            );
            self.type_syntax();
        }
        self.expect_close_delimiter(SyntaxKind::Gt, "expected `>` after generic type arguments");
        self.complete(marker, SyntaxKind::GenericType);
    }

    fn tuple_type(&mut self) {
        let marker = self.start();
        if !self.expect_open_delimiter(SyntaxKind::LParen, "expected `(` before tuple type") {
            self.complete(marker, SyntaxKind::TupleType);
            return;
        }
        if !self.at(SyntaxKind::RParen) {
            self.type_syntax();
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
                if !self.at(SyntaxKind::RParen) {
                    self.type_syntax();
                    while self.at(SyntaxKind::Comma) {
                        self.bump_nontrivia();
                        if self.at(SyntaxKind::RParen) {
                            break;
                        }
                        self.type_syntax();
                    }
                }
            } else {
                self.missing("one-member tuple types require a trailing comma");
            }
        }
        self.expect_close_delimiter(SyntaxKind::RParen, "expected `)` after tuple type");
        self.complete(marker, SyntaxKind::TupleType);
    }

    fn record_type(&mut self) {
        let marker = self.start();
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` before record type") {
            self.complete(marker, SyntaxKind::RecordType);
            return;
        }
        self.record_fields();
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after record type");
        self.complete(marker, SyntaxKind::RecordType);
    }

    fn function_type(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::KwFn, "expected `fn` in function type");
        if !self.expect_open_delimiter(
            SyntaxKind::LParen,
            "expected `(` before function type parameters",
        ) {
            self.complete(marker, SyntaxKind::FunctionType);
            return;
        }
        if !self.at(SyntaxKind::RParen) {
            self.type_syntax();
            while self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
                if self.at(SyntaxKind::RParen) {
                    break;
                }
                self.type_syntax();
            }
        }
        self.expect_close_delimiter(
            SyntaxKind::RParen,
            "expected `)` after function type parameters",
        );
        self.expect(SyntaxKind::KwReturns, "expected `returns` in function type");
        self.type_syntax();
        if self.at(SyntaxKind::KwEffects) {
            self.effect_clause();
        }
        self.complete(marker, SyntaxKind::FunctionType);
    }

    pub(super) fn effect_clause(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::KwEffects, "expected `effects`");
        if !self.expect_open_delimiter(SyntaxKind::LBracket, "expected `[` before effect list") {
            self.complete(marker, SyntaxKind::EffectClause);
            return;
        }
        let mut expect_effect = !self.at(SyntaxKind::RBracket);
        while !self.exceeded
            && !matches!(
                self.nth(0),
                SyntaxKind::RParen
                    | SyntaxKind::RBracket
                    | SyntaxKind::RBrace
                    | SyntaxKind::Gt
                    | SyntaxKind::LBrace
                    | SyntaxKind::Eof
            )
        {
            if self.at(SyntaxKind::Comma) {
                if expect_effect {
                    self.error_one("expected effect ID before `,`");
                } else {
                    self.bump_nontrivia();
                    expect_effect = !self.at(SyntaxKind::RBracket);
                }
            } else {
                if !expect_effect {
                    self.missing("expected `,` between effect IDs");
                }
                self.effect_id();
                expect_effect = false;
            }
        }
        self.expect_close_delimiter(SyntaxKind::RBracket, "expected `]` after effect list");
        self.complete(marker, SyntaxKind::EffectClause);
    }

    fn at_generic_type(&self) -> bool {
        self.generic_type_head().is_some() && self.nth(1) == SyntaxKind::Lt
    }

    fn generic_type_head(&self) -> Option<(&'static str, SyntaxKind, usize)> {
        UNARY_GENERIC_TYPES
            .iter()
            .find(|(spelling, _)| self.at_contextual_type(spelling))
            .map(|(spelling, kind)| (*spelling, *kind, 1))
            .or_else(|| {
                BINARY_GENERIC_TYPES
                    .iter()
                    .find(|(spelling, _)| self.at_contextual_type(spelling))
                    .map(|(spelling, kind)| (*spelling, *kind, 2))
            })
    }

    fn at_contextual_type(&self, spelling: &str) -> bool {
        let mut index = self.pos;
        while let Some(token) = self.tokens.get(index).copied() {
            if !is_trivia(token.kind()) {
                return token.kind() == SyntaxKind::Ident && token.text(self.source) == spelling;
            }
            index += 1;
        }
        false
    }

    fn expect_contextual_type(
        &mut self,
        spelling: &str,
        kind: SyntaxKind,
        message: &'static str,
    ) -> bool {
        if self.at_contextual_type(spelling) {
            self.eat_trivia();
            self.token_as(1, Some(kind));
            true
        } else {
            self.missing(message);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SourceFile, SourceFileId, parse, parse_with_limits};

    fn source(text: &str) -> SourceFile {
        SourceFile::new(SourceFileId::new(23), text).unwrap()
    }

    fn node_kinds(text: &str) -> Vec<SyntaxKind> {
        parse(&source(text))
            .syntax()
            .descendants()
            .map(|node| node.kind())
            .collect()
    }

    #[test]
    fn parses_all_type_and_signature_productions_losslessly() {
        let text = "export record Box { value: List<Option<T>>, callback: fn(T, Map<String, Int>,) returns Result<T, E> effects [io.read@2,] }\n\
export enum Choice { Empty, One(T), Pair(T, U,), Named { left: T right: U, }, }\n\
export async fn run<T: Eq, E: Eq,>(input: Box, callback: fn(T) returns Future<T> effects []) returns Task<Result<T, E>> effects [io.read@2, record.read] {}";
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);

        let kinds = node_kinds(text);
        for kind in [
            SyntaxKind::RecordDeclaration,
            SyntaxKind::EnumDeclaration,
            SyntaxKind::FunctionDeclaration,
            SyntaxKind::GenericParameters,
            SyntaxKind::GenericParameter,
            SyntaxKind::Parameter,
            SyntaxKind::EffectClause,
            SyntaxKind::NamedType,
            SyntaxKind::GenericType,
            SyntaxKind::FunctionType,
        ] {
            assert!(kinds.contains(&kind), "missing {kind:?}");
        }
    }

    #[test]
    fn parses_tuple_and_record_types() {
        let text = "record Types { empty: (), single: (T,), pair: (T, U), shape: { left: T right: List<U>, } }";
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        let kinds = node_kinds(text);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::TupleType)
                .count(),
            3
        );
        assert!(kinds.contains(&SyntaxKind::RecordType));
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn parses_fully_qualified_generated_tool_types() {
        let text = "fn call(input: tools.demo.echo.Input) returns Result<tools.demo.echo.Output, tools.demo.echo.Error> {}";
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);
        assert_eq!(
            parsed
                .syntax()
                .descendants()
                .filter(|node| node.kind() == SyntaxKind::NamedType)
                .map(|node| node.text().to_string().trim().to_owned())
                .collect::<Vec<_>>(),
            [
                "tools.demo.echo.Input",
                "tools.demo.echo.Output",
                "tools.demo.echo.Error",
            ]
        );
    }

    #[test]
    fn prose_type_rules_override_the_mechanical_ebnf_edges() {
        let valid =
            source("record R { callback: fn(T) returns U, builtin: List } fn f() returns Void {}");
        let parsed = parse(&valid);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        let tokens: Vec<_> = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .map(|token| (token.kind(), token.text().to_owned()))
            .collect();
        assert!(tokens.contains(&(SyntaxKind::Ident, "List".to_owned())));
        parsed.assert_round_trip(&valid);

        let invalid = source("record R { singleton: (T) } record Later { ok: T }");
        let parsed = parse(&invalid);
        assert!(parsed.has_errors());
        assert_eq!(
            node_kinds(invalid.text())
                .iter()
                .filter(|kind| **kind == SyntaxKind::RecordDeclaration)
                .count(),
            2
        );
        parsed.assert_round_trip(&invalid);
    }

    #[test]
    fn contextual_type_words_are_reclassified_only_in_type_positions() {
        let text = "record List { Map: List<Map<Result<T, E>, Prompt<U>>> } fn Eq(List: Option<T>) returns Future<Task<T>> {}";
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        let tokens: Vec<_> = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .map(|token| (token.kind(), token.text().to_owned()))
            .collect();
        assert!(tokens.contains(&(SyntaxKind::Ident, "List".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::Ident, "Map".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::KwList, "List".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::KwMapType, "Map".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::KwOption, "Option".to_owned())));
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn rejects_invalid_signatures_and_recovers_to_later_declarations() {
        let text = "enum Empty {} fn bad<T Eq>(x List<T>) returns Map<T> effects [bad@0] {} record Later { ok: T }";
        let source = source(text);
        let parsed = parse(&source);
        assert!(parsed.has_errors());
        assert!(node_kinds(text).contains(&SyntaxKind::RecordDeclaration));
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn deeply_nested_types_obey_parser_resource_limits() {
        let text = format!(
            "fn deep() returns {}T{} {{}}",
            "List<".repeat(80),
            ">".repeat(80)
        );
        let source = source(&text);
        for limits in [
            crate::SyntaxLimits {
                parser_recursion: 12,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                events: 24,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                nodes: 12,
                ..crate::SyntaxLimits::DEFAULT
            },
        ] {
            let parsed = parse_with_limits(&source, limits);
            assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
            assert!(parsed.has_errors());
            parsed.assert_round_trip(&source);
        }
    }

    #[test]
    fn rejects_the_legacy_return_arrow() {
        let source = source("fn legacy() -> Int {}");
        let parsed = parse(&source);
        assert!(parsed.has_errors());
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn range_and_sequence_use_ordinary_unary_generic_type_syntax() {
        let text = "fn consume(values: Sequence<Range<Int>>, window: Range<String>) returns Sequence<Int> { values }";
        let file = source(text);
        let parsed = parse(&file);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&file);
        let tokens = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .map(|token| token.kind())
            .collect::<Vec<_>>();
        assert_eq!(
            tokens
                .iter()
                .filter(|kind| **kind == SyntaxKind::KwSequence)
                .count(),
            2
        );
        assert_eq!(
            tokens
                .iter()
                .filter(|kind| **kind == SyntaxKind::KwRange)
                .count(),
            2
        );
    }
}
