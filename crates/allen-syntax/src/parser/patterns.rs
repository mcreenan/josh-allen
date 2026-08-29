use super::Parser;
use crate::SyntaxKind;

impl Parser<'_, '_> {
    pub(super) fn match_expression(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.expect(SyntaxKind::KwMatch, "expected `match`");
        self.expression_before_body();
        if self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` before match arms") {
            self.match_arms();
            self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after match expression");
        }
        self.complete(marker, SyntaxKind::MatchExpression);
    }

    fn match_arms(&mut self) {
        let mut saw_arm = false;
        let mut expect_arm = true;
        while !self.exceeded && !self.at_match_arm_list_end() {
            if self.at(SyntaxKind::Comma) {
                if expect_arm {
                    self.error_one("expected match arm before `,`");
                } else {
                    self.bump_nontrivia();
                    expect_arm = !self.at(SyntaxKind::RBrace);
                }
                continue;
            }
            if self.at_pattern_start() {
                self.match_arm();
                saw_arm = true;
                expect_arm = false;
            } else {
                self.recover_match_arm();
                expect_arm = false;
            }
        }
        if !saw_arm {
            self.missing("expected match arm");
        }
    }

    fn match_arm(&mut self) {
        let marker = self.start();
        self.pattern();
        self.expect(SyntaxKind::FatArrow, "expected `=>` after match pattern");
        self.expression();
        self.complete(marker, SyntaxKind::MatchArm);
    }

    fn pattern(&mut self) {
        let marker = self.start();
        self.pattern_or();
        self.complete(marker, SyntaxKind::Pattern);
    }

    fn pattern_or(&mut self) {
        let marker = self.start();
        self.pattern_primary();
        while self.at(SyntaxKind::Pipe) {
            self.bump_nontrivia();
            if self.at_pattern_start() {
                self.pattern_primary();
            } else {
                self.missing("expected pattern after `|`");
                break;
            }
        }
        self.complete(marker, SyntaxKind::PatternOr);
    }

    fn pattern_primary(&mut self) {
        let marker = self.start();
        match self.nth(0) {
            SyntaxKind::Minus
                if self.nth(1) == SyntaxKind::IntLiteral
                    && matches!(self.nth(2), SyntaxKind::DotDot | SyntaxKind::DotDotEq) =>
            {
                self.range_pattern();
            }
            SyntaxKind::IntLiteral
            | SyntaxKind::FloatLiteral
            | SyntaxKind::StringLiteral
            | SyntaxKind::BytesLiteral
                if matches!(self.nth(1), SyntaxKind::DotDot | SyntaxKind::DotDotEq) =>
            {
                self.range_pattern();
            }
            SyntaxKind::Underscore | SyntaxKind::KwTrue | SyntaxKind::KwFalse => {
                self.bump_nontrivia();
            }
            SyntaxKind::Ident if self.nth_text(0) == Some("_") => {
                self.bump_pattern_contextual(SyntaxKind::Underscore);
            }
            SyntaxKind::KwSome | SyntaxKind::KwOk | SyntaxKind::KwErr => {
                self.builtin_payload_pattern();
            }
            SyntaxKind::Ident if self.at_contextual_payload_pattern() => {
                self.builtin_payload_pattern();
            }
            SyntaxKind::Ident if self.nth(1) == SyntaxKind::LBrace => self.record_pattern(),
            SyntaxKind::Ident if self.nth(1) == SyntaxKind::Dot => self.enum_pattern(),
            SyntaxKind::Ident if self.nth_text(0) == Some("None") => {
                self.bump_pattern_contextual(SyntaxKind::KwNone);
            }
            SyntaxKind::KwNone | SyntaxKind::Ident => self.bump_nontrivia(),
            _ => self.missing("expected match pattern"),
        }
        self.complete(marker, SyntaxKind::PatternPrimary);
    }

    fn range_pattern(&mut self) {
        let marker = self.start();
        self.range_pattern_endpoint();
        if matches!(self.nth(0), SyntaxKind::DotDot | SyntaxKind::DotDotEq) {
            self.bump_nontrivia();
        } else {
            self.missing("expected `..` or `..=` in range pattern");
        }
        self.range_pattern_endpoint();
        self.complete(marker, SyntaxKind::PatternRange);
    }

    fn range_pattern_endpoint(&mut self) {
        if self.at(SyntaxKind::Minus) {
            self.bump_nontrivia();
            if self.at(SyntaxKind::IntLiteral) {
                self.literal(true);
            } else {
                self.missing("expected Int literal after `-` in range pattern");
            }
            return;
        }
        match self.nth(0) {
            SyntaxKind::IntLiteral
            | SyntaxKind::FloatLiteral
            | SyntaxKind::StringLiteral
            | SyntaxKind::BytesLiteral => self.literal(false),
            _ => self.missing("expected literal range-pattern endpoint"),
        }
    }

    fn builtin_payload_pattern(&mut self) {
        let kind = match self.nth_text(0) {
            Some("Some") => SyntaxKind::KwSome,
            Some("Ok") => SyntaxKind::KwOk,
            Some("Err") => SyntaxKind::KwErr,
            _ => self.nth(0),
        };
        if self.nth(0) == SyntaxKind::Ident {
            self.bump_pattern_contextual(kind);
        } else {
            self.bump_nontrivia();
        }
        if self.expect_open_delimiter(
            SyntaxKind::LParen,
            "expected `(` after built-in pattern constructor",
        ) {
            self.pattern();
            self.recover_builtin_pattern_payload(SyntaxKind::RParen);
            self.expect_close_delimiter(
                SyntaxKind::RParen,
                "expected `)` after built-in pattern binding",
            );
        }
    }

    fn record_pattern(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::Ident, "expected record type name");
        if self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` before record pattern") {
            self.pattern_fields();
            self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after record pattern");
        }
        self.complete(marker, SyntaxKind::RecordPattern);
    }

    fn enum_pattern(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::Ident, "expected enum type name");
        self.expect(SyntaxKind::Dot, "expected `.` before enum variant");
        self.expect(SyntaxKind::Ident, "expected enum variant name");
        if self.at(SyntaxKind::LParen) {
            self.enum_tuple_pattern();
        } else if self.at(SyntaxKind::LBrace) {
            self.expect_open_delimiter(
                SyntaxKind::LBrace,
                "expected `{` before enum record pattern",
            );
            self.pattern_fields();
            self.expect_close_delimiter(
                SyntaxKind::RBrace,
                "expected `}` after enum record pattern",
            );
        }
        self.complete(marker, SyntaxKind::EnumPattern);
    }

    fn enum_tuple_pattern(&mut self) {
        if !self.expect_open_delimiter(SyntaxKind::LParen, "expected `(` before enum tuple pattern")
        {
            return;
        }
        let mut saw_pattern = false;
        let mut expect_pattern = true;
        while !self.exceeded && !self.at_pattern_list_end(SyntaxKind::RParen) {
            if expect_pattern {
                if self.at_pattern_start() {
                    self.pattern();
                    saw_pattern = true;
                    expect_pattern = false;
                } else {
                    self.error_one("expected enum payload pattern");
                }
            } else if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
                expect_pattern = !self.at(SyntaxKind::RParen);
            } else if self.at_pattern_start() {
                self.missing("expected `,` between enum payload patterns");
                self.pattern();
            } else {
                self.error_one("expected `,` or `)` in enum tuple pattern");
            }
        }
        if !saw_pattern {
            self.missing("expected enum payload pattern");
        }
        self.expect_close_delimiter(SyntaxKind::RParen, "expected `)` after enum tuple pattern");
    }

    fn pattern_fields(&mut self) {
        let mut expect_field = !self.at(SyntaxKind::RBrace);
        while !self.exceeded && !self.at_pattern_list_end(SyntaxKind::RBrace) {
            if self.at(SyntaxKind::Comma) {
                if expect_field {
                    self.error_one("expected pattern field before `,`");
                } else {
                    self.bump_nontrivia();
                    expect_field = !self.at(SyntaxKind::RBrace);
                }
            } else if self.at(SyntaxKind::Ident) {
                self.pattern_field();
                expect_field = false;
            } else {
                self.error_one("expected pattern field");
                expect_field = false;
            }
        }
    }

    fn pattern_field(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::Ident, "expected pattern field name");
        if self.at(SyntaxKind::Colon) {
            self.bump_nontrivia();
            self.pattern();
        }
        self.complete(marker, SyntaxKind::PatternField);
    }

    pub(super) fn at_pattern_start(&self) -> bool {
        matches!(
            self.nth(0),
            SyntaxKind::Underscore
                | SyntaxKind::Ident
                | SyntaxKind::IntLiteral
                | SyntaxKind::FloatLiteral
                | SyntaxKind::StringLiteral
                | SyntaxKind::BytesLiteral
                | SyntaxKind::KwTrue
                | SyntaxKind::KwFalse
                | SyntaxKind::KwNone
                | SyntaxKind::KwSome
                | SyntaxKind::KwOk
                | SyntaxKind::KwErr
                | SyntaxKind::Minus
        )
    }

    fn at_contextual_payload_pattern(&self) -> bool {
        matches!(self.nth_text(0), Some("Some" | "Ok" | "Err")) && self.nth(1) == SyntaxKind::LParen
    }

    fn at_pattern_list_end(&self, close: SyntaxKind) -> bool {
        matches!(
            self.nth(0),
            SyntaxKind::FatArrow
                | SyntaxKind::RParen
                | SyntaxKind::RBracket
                | SyntaxKind::RBrace
                | SyntaxKind::Eof
        ) || self.at(close)
    }

    fn recover_builtin_pattern_payload(&mut self, close: SyntaxKind) {
        let saved_depth = self.delimiter_depth;
        let mut closers = Vec::new();
        while !self.exceeded && !self.at(SyntaxKind::Eof) {
            let kind = self.nth(0);
            if closers.is_empty() {
                if kind == close
                    || matches!(
                        kind,
                        SyntaxKind::FatArrow | SyntaxKind::RBrace | SyntaxKind::RBracket
                    )
                {
                    break;
                }
            } else if closers.last().is_some_and(|expected| *expected == kind) {
                closers.pop();
                self.exit_delimiter();
                self.error_one("unexpected built-in pattern payload syntax");
                continue;
            }
            if let Some(nested_close) = match_recovery_closer(kind) {
                if !self.enter_delimiter() {
                    self.delimiter_depth = saved_depth;
                    return;
                }
                closers.push(nested_close);
            } else if matches!(
                kind,
                SyntaxKind::RParen | SyntaxKind::RBracket | SyntaxKind::RBrace
            ) {
                break;
            }
            self.error_one("unexpected built-in pattern payload syntax");
        }
        self.delimiter_depth = saved_depth;
    }

    fn recover_match_arm(&mut self) {
        let marker = self.start();
        let saved_depth = self.delimiter_depth;
        let mut closers = Vec::new();
        let mut consumed = false;
        while !self.exceeded && !self.at(SyntaxKind::Eof) {
            let kind = self.nth(0);
            if closers.is_empty()
                && (matches!(
                    kind,
                    SyntaxKind::Comma
                        | SyntaxKind::RParen
                        | SyntaxKind::RBracket
                        | SyntaxKind::RBrace
                ) || consumed && self.at_pattern_start())
            {
                break;
            }
            if closers.last().is_some_and(|expected| *expected == kind) {
                closers.pop();
                self.exit_delimiter();
                self.bump_nontrivia();
                consumed = true;
                continue;
            }
            if let Some(close) = match_recovery_closer(kind) {
                if !self.enter_delimiter() {
                    self.delimiter_depth = saved_depth;
                    return;
                }
                closers.push(close);
            } else if kind == SyntaxKind::RBrace {
                break;
            }
            self.bump_nontrivia();
            consumed = true;
        }
        self.delimiter_depth = saved_depth;
        if consumed {
            self.complete(marker, SyntaxKind::Error);
            self.error("unexpected match arm syntax");
        } else {
            self.complete(marker, SyntaxKind::Missing);
            self.error("expected match arm");
        }
    }

    fn bump_pattern_contextual(&mut self, kind: SyntaxKind) {
        self.eat_trivia();
        self.token_as(1, Some(kind));
    }

    fn at_match_arm_list_end(&self) -> bool {
        matches!(
            self.nth(0),
            SyntaxKind::RParen | SyntaxKind::RBracket | SyntaxKind::RBrace | SyntaxKind::Eof
        )
    }
}

fn match_recovery_closer(kind: SyntaxKind) -> Option<SyntaxKind> {
    match kind {
        SyntaxKind::LParen => Some(SyntaxKind::RParen),
        SyntaxKind::LBracket => Some(SyntaxKind::RBracket),
        SyntaxKind::LBrace | SyntaxKind::TemplateExprStart => Some(SyntaxKind::RBrace),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SourceFile, SourceFileId, SyntaxLimits, lex, parse, parse_with_limits};

    fn source(text: &str) -> SourceFile {
        SourceFile::new(SourceFileId::new(37), text).unwrap()
    }

    fn node_kinds(text: &str) -> Vec<SyntaxKind> {
        parse(&source(text))
            .syntax()
            .descendants()
            .map(|node| node.kind())
            .collect()
    }

    #[test]
    fn parses_every_match_pattern_and_closure_production_losslessly() {
        let text = r"fn classify(value: Reading) returns Int {
  let apply = fn(callback: fn({ value: Int }) returns Int, input: Int,) returns Result<Int, E> effects [tool.run@2, io.read] { Ok(callback({ value: input })) };
  match value {
    _ => 0
    true => 1,
    false => 0
    None => 0,
    Some(item) => item
    Ok(_) => 1,
    Err(error) => 0
    Box { value, label: _, } => value,
    Reading.Empty => 0
    Reading.Number(number,) => number,
    Reading.Pair(left, _, right,) => left
    Reading.Named { value: renamed label, ignored: _, } => renamed,
    Some.Variant => 0
    Ok { value } => value,
  }
}";
        let file = source(text);
        let parsed = parse(&file);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&file);

        let kinds = node_kinds(text);
        for kind in [
            SyntaxKind::MatchExpression,
            SyntaxKind::MatchArm,
            SyntaxKind::Pattern,
            SyntaxKind::RecordPattern,
            SyntaxKind::EnumPattern,
            SyntaxKind::PatternField,
            SyntaxKind::Closure,
            SyntaxKind::Parameter,
            SyntaxKind::EffectClause,
        ] {
            assert!(kinds.contains(&kind), "missing {kind:?}");
        }
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::MatchArm)
                .count(),
            14
        );

        let tokens: Vec<_> = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .map(|token| (token.kind(), token.text().to_owned()))
            .collect();
        for (kind, spelling) in [
            (SyntaxKind::KwNone, "None"),
            (SyntaxKind::KwSome, "Some"),
            (SyntaxKind::KwOk, "Ok"),
            (SyntaxKind::KwErr, "Err"),
            (SyntaxKind::Underscore, "_"),
            (SyntaxKind::EffectId, "tool.run@2"),
            (SyntaxKind::EffectId, "io.read"),
        ] {
            assert!(
                tokens.contains(&(kind, spelling.to_owned())),
                "missing {kind:?} {spelling}"
            );
        }
        assert!(tokens.contains(&(SyntaxKind::Ident, "Some".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::Ident, "Ok".to_owned())));
    }

    #[test]
    fn match_scrutinee_body_boundary_keeps_nested_record_constructors() {
        let text =
            "fn f(value: Int) returns Int { match choose(Point { x: value }) { _ => value } }";
        let file = source(text);
        let parsed = parse(&file);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&file);

        let kinds = node_kinds(text);
        assert!(kinds.contains(&SyntaxKind::RecordConstructor));
        assert!(kinds.contains(&SyntaxKind::MatchExpression));
        assert!(kinds.contains(&SyntaxKind::MatchArm));
    }

    #[test]
    fn qualified_match_scrutinees_do_not_capture_the_arm_brace() {
        for text in [
            "fn f() returns Int { match Reading.Empty { _ => 0 } }",
            "fn f(value: Reading) returns Int { match value.kind { _ => 0 } }",
        ] {
            let source = source(text);
            let parsed = parse(&source);
            assert!(
                parsed.diagnostics().is_empty(),
                "{:?}",
                parsed.diagnostics()
            );
            assert!(!parsed.has_errors(), "rejected {text}");
            parsed.assert_round_trip(&source);
            let kinds = node_kinds(text);
            assert!(kinds.contains(&SyntaxKind::MatchArm));
            assert!(!kinds.contains(&SyntaxKind::EnumRecordConstructor));
        }

        let text = "fn f() returns Int { match choose(Reading.Named { value: 1 }) { _ => 0 } }";
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);
        assert!(node_kinds(text).contains(&SyntaxKind::EnumRecordConstructor));
    }

    #[test]
    fn none_is_contextual_only_for_the_bare_builtin_pattern() {
        let text = "fn f(value: T) returns Int { match value { None => 0, None.Variant => 1, None { field } => 2 } }";
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
        assert!(kinds.contains(&SyntaxKind::EnumPattern));
        assert!(kinds.contains(&SyntaxKind::RecordPattern));
        let none_tokens: Vec<_> = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .filter(|token| token.text() == "None")
            .map(|token| token.kind())
            .collect();
        assert_eq!(
            none_tokens,
            [SyntaxKind::KwNone, SyntaxKind::Ident, SyntaxKind::Ident]
        );
    }

    #[test]
    fn malformed_patterns_and_arm_separators_recover_to_later_arms_and_declarations() {
        let text = "fn bad(value: Reading) returns Int { match value { , Some(,) =>, Bogus => choose(Nested { value: [0] }),, Reading.Pair(left right) => left, Box { , value: } => 1, _ => 2 } } fn later() returns Void {}";
        let source = source(text);
        let parsed = parse(&source);
        assert!(parsed.has_errors());
        assert!(
            parsed.diagnostics().len() <= 20,
            "{:?}",
            parsed.diagnostics()
        );
        parsed.assert_round_trip(&source);

        let kinds = node_kinds(text);
        assert!(kinds.contains(&SyntaxKind::Error));
        assert!(kinds.contains(&SyntaxKind::Missing));
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::RecordPattern)
                .count(),
            1,
            "nested malformed-arm expressions must not become arm patterns"
        );
        assert!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::MatchArm)
                .count()
                >= 4
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::FunctionDeclaration)
                .count(),
            2
        );
    }

    #[test]
    fn unclosed_call_recovery_stops_before_the_next_match_arm() {
        let text = "fn f(value: T) returns Int { match value { None => call(1, Some(item) => 2, _ => 0 } } fn later() returns Void {}";
        let file = source(text);
        let parsed = parse(&file);
        assert!(parsed.has_errors());
        parsed.assert_round_trip(&file);

        let root = parsed.syntax();
        assert_eq!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::MatchArm)
                .count(),
            3,
            "the unclosed call swallowed a later arm"
        );
        assert_eq!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                .count(),
            2
        );
        let call = root
            .descendants()
            .find(|node| {
                node.kind() == SyntaxKind::Postfix
                    && node
                        .descendants_with_tokens()
                        .filter_map(rowan::NodeOrToken::into_token)
                        .find(|token| {
                            !matches!(
                                token.kind(),
                                SyntaxKind::Whitespace
                                    | SyntaxKind::Newline
                                    | SyntaxKind::LineComment
                                    | SyntaxKind::BlockComment
                            )
                        })
                        .is_some_and(|token| token.text() == "call")
            })
            .expect("malformed call remains structured");
        assert!(
            !call
                .descendants_with_tokens()
                .filter_map(rowan::NodeOrToken::into_token)
                .any(|token| token.kind() == SyntaxKind::FatArrow),
            "call recovery consumed a match-arm arrow"
        );
    }

    #[test]
    fn malformed_closure_lists_recover_through_the_body_and_later_declaration() {
        let text = "fn f() returns Void { let bad = fn(, first: Int second: Int,,) returns Int effects [,tool.run@2 io.read,,] { first }; return; } fn later() returns Void {}";
        let source = source(text);
        let parsed = parse(&source);
        assert!(parsed.has_errors());
        assert!(
            parsed.diagnostics().len() <= 12,
            "{:?}",
            parsed.diagnostics()
        );
        parsed.assert_round_trip(&source);

        let kinds = node_kinds(text);
        assert!(kinds.contains(&SyntaxKind::Closure));
        assert!(kinds.contains(&SyntaxKind::EffectClause));
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::FunctionDeclaration)
                .count(),
            2
        );
        let effect_ids: Vec<_> = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .filter(|token| token.kind() == SyntaxKind::EffectId)
            .map(|token| token.text().to_owned())
            .collect();
        assert_eq!(effect_ids, ["tool.run@2", "io.read"]);
    }

    #[test]
    fn parameter_and_effect_recovery_preserve_foreign_enclosing_closers() {
        for text in [
            "fn bad(x: Int ] fn later() returns Void {}",
            "fn bad(x: Int > fn later() returns Void {}",
            "fn bad() returns Void effects [io.read ) fn later() returns Void {}",
            "fn bad() returns Void effects [io.read > fn later() returns Void {}",
        ] {
            let source = source(text);
            let parsed = parse(&source);
            assert!(parsed.has_errors(), "accepted {text}");
            assert!(
                parsed.diagnostics().len() <= 8,
                "{:?}",
                parsed.diagnostics()
            );
            parsed.assert_round_trip(&source);
            assert_eq!(
                node_kinds(text)
                    .iter()
                    .filter(|kind| **kind == SyntaxKind::FunctionDeclaration)
                    .count(),
                2,
                "surrounding declaration was swallowed for {text}"
            );
        }
    }

    #[test]
    fn malformed_builtin_payload_recovery_tracks_matching_depth_and_limits() {
        let text = "fn f(value: T) returns Int { match value { Some((x => y)) => 1, _ => 0 } } fn later() returns Void {}";
        let file = source(text);
        let parsed = parse(&file);
        assert!(parsed.has_errors());
        assert!(
            parsed.diagnostics().len() <= 8,
            "{:?}",
            parsed.diagnostics()
        );
        parsed.assert_round_trip(&file);
        let kinds = node_kinds(text);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::MatchArm)
                .count(),
            2
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::FunctionDeclaration)
                .count(),
            2
        );

        let deep =
            source("fn f(value: T) returns Int { match value { Some((((x)))) => 1, _ => 0 } }");
        let parsed = parse_with_limits(
            &deep,
            SyntaxLimits {
                delimiter_depth: 4,
                ..SyntaxLimits::DEFAULT
            },
        );
        assert!(parsed.has_errors());
        assert!(
            parsed
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.code() == "S0100"),
            "{:?}",
            parsed.diagnostics()
        );
        parsed.assert_round_trip(&deep);
    }

    #[test]
    fn flat_match_arm_recovery_preserves_foreign_parent_and_list_closers() {
        for (text, closer, enclosing_kind) in [
            (
                "fn bad(value: T) returns Int { let recovered = (match value { Bogus => 0); return 1; } fn later() returns Void {}",
                SyntaxKind::RParen,
                SyntaxKind::TupleOrGroup,
            ),
            (
                "fn bad(value: T) returns Int { let recovered = [match value { Bogus => 0]; return 1; } fn later() returns Void {}",
                SyntaxKind::RBracket,
                SyntaxKind::ListLiteral,
            ),
        ] {
            let source = source(text);
            let parsed = parse(&source);
            assert!(parsed.has_errors());
            assert!(
                parsed.diagnostics().len() <= 4,
                "{:?}",
                parsed.diagnostics()
            );
            parsed.assert_round_trip(&source);

            let root = parsed.syntax();
            let match_expression = root
                .descendants()
                .find(|node| node.kind() == SyntaxKind::MatchExpression)
                .expect("malformed match expression remains structured");
            assert!(
                !match_expression
                    .descendants_with_tokens()
                    .filter_map(rowan::NodeOrToken::into_token)
                    .any(|token| token.kind() == closer),
                "match recovery consumed foreign {closer:?}"
            );
            let enclosing = root
                .descendants()
                .find(|node| node.kind() == enclosing_kind)
                .expect("enclosing expression remains structured");
            assert!(
                enclosing
                    .descendants_with_tokens()
                    .filter_map(rowan::NodeOrToken::into_token)
                    .any(|token| token.kind() == closer),
                "enclosing expression lost {closer:?}"
            );
            assert_eq!(
                root.descendants()
                    .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                    .count(),
                2,
                "recovery did not reach later function"
            );
        }
    }

    #[test]
    fn match_pattern_and_closure_truncations_terminate_and_round_trip() {
        let text = "// π\nfn f(value: Reading) returns Int { let c = fn(x: Int,) returns Int effects [tool.run@2,] { x }; match value { Reading.Named { value, } => c(value), Some(_) => 1, _ => 0, } }";
        let full = source(text);
        let token_ends: Vec<_> = lex(&full)
            .tokens()
            .iter()
            .map(|token| u32::from(token.range().end()) as usize)
            .collect();
        for end in token_ends {
            let prefix = source(&text[..end]);
            let parsed = parse(&prefix);
            assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
            assert!(parsed.diagnostics().len() <= SyntaxLimits::DEFAULT.diagnostics);
            parsed.assert_round_trip(&prefix);
        }
    }

    #[test]
    fn match_pattern_and_closure_obey_injected_parser_limits() {
        let text = "fn f(value: Reading) returns Int { match value { Box { value: inner } => fn(arg: Int) returns Int effects [tool.run@2] { match arg { _ => arg } }(inner), _ => 0 } }";
        let file = source(text);
        let parsed = parse(&file);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&file);

        for limits in [
            SyntaxLimits {
                tokens: 8,
                ..SyntaxLimits::DEFAULT
            },
            SyntaxLimits {
                parser_recursion: 8,
                ..SyntaxLimits::DEFAULT
            },
            SyntaxLimits {
                delimiter_depth: 2,
                ..SyntaxLimits::DEFAULT
            },
            SyntaxLimits {
                events: 32,
                ..SyntaxLimits::DEFAULT
            },
            SyntaxLimits {
                nodes: 16,
                ..SyntaxLimits::DEFAULT
            },
        ] {
            let parsed = parse_with_limits(&file, limits);
            assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
            assert!(parsed.has_errors(), "limit did not trigger: {limits:?}");
            parsed.assert_round_trip(&file);
        }

        let malformed = source("fn f(value: T) returns Int { match value { Some(,) =>, _ => 0 } }");
        let parsed = parse_with_limits(
            &malformed,
            SyntaxLimits {
                diagnostics: 1,
                ..SyntaxLimits::DEFAULT
            },
        );
        assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
        assert!(parsed.has_errors());
        assert_eq!(parsed.diagnostics().len(), 1);
        parsed.assert_round_trip(&malformed);
    }

    #[test]
    fn range_and_or_patterns_are_low_precedence_and_recursive() {
        let text = "fn classify(value: T) returns Int { match value { -5..-1 | -1..=3 | 5..8 => 1, Some(1..=2 | item) => 2, Choice.Pair(0..1 | left, right) => 3, Box { value: 0..=9 | inner } => 4, _ => 0 } }";
        let file = source(text);
        let parsed = parse(&file);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&file);
        let kinds = node_kinds(text);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::PatternRange)
                .count(),
            6
        );
        assert!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::PatternOr)
                .count()
                >= 9
        );

        let malformed = source(
            "fn bad(value: T) returns Int { match value { 1.. => 0, _ => 1 } } fn later() returns Void {}",
        );
        let parsed = parse(&malformed);
        assert!(parsed.has_errors());
        assert_eq!(
            parsed
                .syntax()
                .descendants()
                .filter(|node| node.kind() == SyntaxKind::MatchArm)
                .count(),
            2
        );
        assert_eq!(
            parsed
                .syntax()
                .descendants()
                .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                .count(),
            2
        );
        parsed.assert_round_trip(&malformed);
    }
}
