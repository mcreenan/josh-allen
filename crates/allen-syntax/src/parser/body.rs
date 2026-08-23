use super::Parser;
use crate::SyntaxKind;

impl Parser<'_, '_> {
    pub(super) fn body(&mut self) {
        let marker = self.start();
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{`") {
            self.complete(marker, SyntaxKind::Body);
            return;
        }
        while !self.exceeded && !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            if self.at(SyntaxKind::KwIf) {
                if self.conditional_has_following_body_syntax() {
                    self.statement();
                } else {
                    self.expression();
                    break;
                }
            } else if self.at_statement_start() {
                self.statement();
            } else if self.at_expression_start() {
                self.expression();
                break;
            } else {
                self.error_one("expected statement or body expression");
            }
        }
        if !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            self.error_until_body_end("expected `}` after body expression");
        }
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after function body");
        self.complete(marker, SyntaxKind::Body);
    }

    fn statement(&mut self) {
        let marker = self.start();
        match self.nth(0) {
            SyntaxKind::KwLet | SyntaxKind::KwMut => self.local_declaration_statement(),
            SyntaxKind::Ident if self.at_assignment_statement() => self.assignment_statement(),
            SyntaxKind::KwReturn => self.return_statement(),
            SyntaxKind::KwBreak => self.keyword_statement("expected `;` after `break`"),
            SyntaxKind::KwContinue => self.keyword_statement("expected `;` after `continue`"),
            SyntaxKind::KwIf => self.conditional_expression(),
            SyntaxKind::KwWhile => self.while_statement(),
            SyntaxKind::KwLoop => self.loop_statement(),
            SyntaxKind::KwFor => self.for_statement(),
            _ => self.error_one("expected statement"),
        }
        self.complete(marker, SyntaxKind::Statement);
    }

    fn local_declaration_statement(&mut self) {
        self.bump_nontrivia();
        self.expect(SyntaxKind::Ident, "expected local name");
        if self.at(SyntaxKind::Colon) {
            self.bump_nontrivia();
            self.type_syntax();
        }
        self.expect(SyntaxKind::Eq, "expected `=` after local declaration");
        self.expression();
        self.expect(SyntaxKind::Semi, "expected `;` after local declaration");
    }

    fn assignment_statement(&mut self) {
        self.expect(SyntaxKind::Ident, "expected assignment target");
        if self.at_assignment_operator() {
            self.bump_nontrivia();
        } else {
            self.missing("expected assignment operator");
        }
        self.expression();
        self.expect(SyntaxKind::Semi, "expected `;` after assignment");
    }

    fn return_statement(&mut self) {
        self.bump_nontrivia();
        if !self.at(SyntaxKind::Semi) && !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            self.expression();
        }
        self.expect(SyntaxKind::Semi, "expected `;` after `return`");
    }

    fn keyword_statement(&mut self, semi_message: &'static str) {
        self.bump_nontrivia();
        self.expect(SyntaxKind::Semi, semi_message);
    }

    fn while_statement(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::KwWhile, "expected `while`");
        if self.expect_open_delimiter(SyntaxKind::LParen, "expected `(` after `while`") {
            self.expression();
            self.expect_close_delimiter(SyntaxKind::RParen, "expected `)` after while condition");
        }
        if self.at(SyntaxKind::LBrace) {
            self.body();
        } else {
            self.missing("expected while body");
        }
        self.complete(marker, SyntaxKind::WhileStatement);
    }

    fn loop_statement(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::KwLoop, "expected `loop`");
        if self.at(SyntaxKind::LBrace) {
            self.body();
        } else {
            self.missing("expected loop body");
        }
        self.complete(marker, SyntaxKind::LoopStatement);
    }

    fn for_statement(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::KwFor, "expected `for`");
        self.loop_binding();
        self.expect(SyntaxKind::KwIn, "expected `in` after loop binding");
        self.expression_before_body();
        if self.at(SyntaxKind::DotDot) {
            self.bump_nontrivia();
            self.expression_before_body();
        }
        if self.at(SyntaxKind::LBrace) {
            self.body();
        } else {
            self.missing("expected for body");
        }
        self.complete(marker, SyntaxKind::ForStatement);
    }

    fn loop_binding(&mut self) {
        let marker = self.start();
        if self.at_loop_binding_item() {
            self.bump_loop_binding_item();
        } else if self.at(SyntaxKind::LParen) {
            self.expect_open_delimiter(SyntaxKind::LParen, "expected loop binding");
            if self.at(SyntaxKind::RParen) {
                self.missing("expected tuple loop binding item");
            } else {
                if self.at_loop_binding_item() {
                    self.loop_binding_item();
                } else {
                    self.missing("expected tuple loop binding item");
                }
                if self.at(SyntaxKind::Comma) {
                    self.bump_nontrivia();
                } else {
                    self.missing("expected `,` in tuple loop binding");
                }
                while !self.exceeded && !self.at_loop_binding_end() {
                    if self.at_loop_binding_item() {
                        self.loop_binding_item();
                    } else {
                        self.error_one("expected tuple loop binding item");
                    }
                    if self.at(SyntaxKind::Comma) {
                        self.bump_nontrivia();
                    } else if !self.at_loop_binding_end() {
                        self.missing("expected `,` in tuple loop binding");
                    }
                }
            }
            self.expect_close_delimiter(SyntaxKind::RParen, "expected `)` after loop binding");
        } else {
            self.missing("expected loop binding");
        }
        self.complete(marker, SyntaxKind::LoopBinding);
    }

    fn loop_binding_item(&mut self) {
        let marker = self.start();
        if self.at_loop_binding_item() {
            self.bump_loop_binding_item();
        } else {
            self.missing("expected loop binding item");
        }
        self.complete(marker, SyntaxKind::LoopBindingItem);
    }

    fn bump_loop_binding_item(&mut self) {
        if self.nth_text(0) == Some("_") {
            self.eat_trivia();
            self.token_as(1, Some(SyntaxKind::Underscore));
        } else {
            self.bump_nontrivia();
        }
    }

    fn at_loop_binding_item(&self) -> bool {
        matches!(self.nth(0), SyntaxKind::Ident | SyntaxKind::Underscore)
    }

    fn at_loop_binding_end(&self) -> bool {
        matches!(
            self.nth(0),
            SyntaxKind::RParen
                | SyntaxKind::RBracket
                | SyntaxKind::RBrace
                | SyntaxKind::KwIn
                | SyntaxKind::LBrace
                | SyntaxKind::Eof
        )
    }

    fn at_statement_start(&self) -> bool {
        matches!(
            self.nth(0),
            SyntaxKind::KwLet
                | SyntaxKind::KwMut
                | SyntaxKind::KwReturn
                | SyntaxKind::KwBreak
                | SyntaxKind::KwContinue
                | SyntaxKind::KwIf
                | SyntaxKind::KwWhile
                | SyntaxKind::KwLoop
                | SyntaxKind::KwFor
        ) || self.at_assignment_statement()
    }

    fn at_assignment_statement(&self) -> bool {
        self.nth(0) == SyntaxKind::Ident && is_assignment_operator(self.nth(1))
    }

    fn at_assignment_operator(&self) -> bool {
        is_assignment_operator(self.nth(0))
    }

    fn conditional_has_following_body_syntax(&self) -> bool {
        let Some(start) = self.next_nontrivia_index(self.pos) else {
            return true;
        };
        let Some((after, has_else)) = self.after_conditional_index(start) else {
            return true;
        };
        let Some(next) = self.next_nontrivia_index(after) else {
            return false;
        };
        let kind = self.tokens[next].kind();
        let continues_expression = if has_else {
            is_conditional_expression_continuation(kind)
        } else {
            is_conditional_binary_continuation(kind)
        };
        !matches!(kind, SyntaxKind::RBrace | SyntaxKind::Eof) && !continues_expression
    }

    fn after_conditional_index(&self, mut index: usize) -> Option<(usize, bool)> {
        let mut has_else = false;
        loop {
            index = self.next_nontrivia_index(index)?;
            if self.tokens.get(index)?.kind() != SyntaxKind::KwIf {
                return None;
            }
            index = self.next_nontrivia_index(index + 1)?;
            index = self.skip_balanced_index(index, SyntaxKind::LParen, SyntaxKind::RParen)?;
            index = self.next_nontrivia_index(index)?;
            index = self.skip_balanced_index(index, SyntaxKind::LBrace, SyntaxKind::RBrace)?;
            let Some(next) = self.next_nontrivia_index(index) else {
                return Some((index, has_else));
            };
            if self.tokens[next].kind() != SyntaxKind::KwElse {
                return Some((index, has_else));
            }
            has_else = true;
            let after_else = self.next_nontrivia_index(next + 1)?;
            if self.tokens[after_else].kind() == SyntaxKind::KwIf {
                index = after_else;
                continue;
            }
            return self
                .skip_balanced_index(after_else, SyntaxKind::LBrace, SyntaxKind::RBrace)
                .map(|after| (after, has_else));
        }
    }

    fn skip_balanced_index(
        &self,
        mut index: usize,
        open: SyntaxKind,
        close: SyntaxKind,
    ) -> Option<usize> {
        index = self.next_nontrivia_index(index)?;
        if self.tokens.get(index)?.kind() != open {
            return None;
        }
        let mut closers = vec![close];
        let mut cursor = index + 1;
        while let Some(token) = self.tokens.get(cursor) {
            let kind = token.kind();
            if super::is_trivia(kind) {
                cursor += 1;
                continue;
            }
            if closers.last().is_some_and(|expected| *expected == kind) {
                closers.pop();
                cursor += 1;
                if closers.is_empty() {
                    return Some(cursor);
                }
                continue;
            }
            if let Some(nested) = mixed_lookahead_closer(kind) {
                if closers.len() >= self.limits.delimiter_depth {
                    return None;
                }
                closers.push(nested);
            } else if matches!(
                kind,
                SyntaxKind::RParen | SyntaxKind::RBracket | SyntaxKind::RBrace | SyntaxKind::Eof
            ) {
                return None;
            }
            cursor += 1;
        }
        None
    }

    fn next_nontrivia_index(&self, mut index: usize) -> Option<usize> {
        while let Some(token) = self.tokens.get(index) {
            if !super::is_trivia(token.kind()) {
                return Some(index);
            }
            index += 1;
        }
        None
    }

    fn error_until_body_end(&mut self, message: &'static str) {
        let marker = self.start();
        while !self.exceeded && !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            self.bump();
        }
        self.complete(marker, SyntaxKind::Error);
        self.error(message);
    }
}

fn mixed_lookahead_closer(kind: SyntaxKind) -> Option<SyntaxKind> {
    match kind {
        SyntaxKind::LParen => Some(SyntaxKind::RParen),
        SyntaxKind::LBracket => Some(SyntaxKind::RBracket),
        SyntaxKind::LBrace | SyntaxKind::TemplateExprStart => Some(SyntaxKind::RBrace),
        _ => None,
    }
}

fn is_conditional_expression_continuation(kind: SyntaxKind) -> bool {
    is_conditional_binary_continuation(kind)
        || matches!(
            kind,
            SyntaxKind::LBracket | SyntaxKind::Dot | SyntaxKind::LParen | SyntaxKind::Question
        )
}

fn is_conditional_binary_continuation(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::PipePipe
            | SyntaxKind::AmpAmp
            | SyntaxKind::EqEq
            | SyntaxKind::NotEq
            | SyntaxKind::Lt
            | SyntaxKind::LtEq
            | SyntaxKind::Gt
            | SyntaxKind::GtEq
            | SyntaxKind::Plus
            | SyntaxKind::Minus
            | SyntaxKind::Star
            | SyntaxKind::Slash
            | SyntaxKind::Percent
    )
}

fn is_assignment_operator(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Eq
            | SyntaxKind::PlusEq
            | SyntaxKind::MinusEq
            | SyntaxKind::StarEq
            | SyntaxKind::SlashEq
            | SyntaxKind::PercentEq
    )
}

#[cfg(test)]
mod tests {
    use crate::{SourceFile, SourceFileId, SyntaxKind, parse};

    fn source(text: &str) -> SourceFile {
        SourceFile::new(SourceFileId::new(41), text).unwrap()
    }

    #[test]
    fn for_iterables_preserve_body_braces_and_nested_record_constructors() {
        let text = "fn f() returns Void { for item in values { return; } for index in start..end { continue; } for point in choose(Point { value: 1 }) { break; } }";
        let file = source(text);
        let parsed = parse(&file);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&file);

        let kinds: Vec<_> = parsed
            .syntax()
            .descendants()
            .map(|node| node.kind())
            .collect();
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::ForStatement)
                .count(),
            3
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::RecordConstructor)
                .count(),
            1,
            "nested constructors remain valid inside an iterable"
        );
    }

    #[test]
    fn loop_binding_wildcards_are_contextual_and_tuple_shapes_are_exact() {
        let valid =
            source("fn f() returns Void { for _ in values {} for (_, _, item,) in tuples {} }");
        let parsed = parse(&valid);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&valid);
        assert_eq!(
            parsed
                .syntax()
                .descendants_with_tokens()
                .filter_map(rowan::NodeOrToken::into_token)
                .filter(|token| token.kind() == SyntaxKind::Underscore)
                .count(),
            3
        );

        for text in [
            "fn bad() returns Void { for () in values {} } fn later() returns Void {}",
            "fn bad() returns Void { for (item) in values {} } fn later() returns Void {}",
            "fn bad() returns Void { for (, item) in values {} } fn later() returns Void {}",
        ] {
            let file = source(text);
            let parsed = parse(&file);
            assert!(parsed.has_errors(), "accepted {text}");
            parsed.assert_round_trip(&file);
            assert_eq!(
                parsed
                    .syntax()
                    .descendants()
                    .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                    .count(),
                2,
                "loop-binding recovery swallowed the later declaration for {text}"
            );
        }
    }

    #[test]
    fn unterminated_tuple_loop_binding_preserves_the_header_body_and_later_declaration() {
        let text = "fn bad() returns Void { for (first, second in values { return; } } fn later() returns Void {}";
        let file = source(text);
        let parsed = parse(&file);
        assert!(parsed.has_errors());
        parsed.assert_round_trip(&file);

        let root = parsed.syntax();
        let binding = root
            .descendants()
            .find(|node| node.kind() == SyntaxKind::LoopBinding)
            .expect("malformed tuple binding remains structured");
        assert!(
            !binding
                .descendants_with_tokens()
                .filter_map(rowan::NodeOrToken::into_token)
                .any(|token| matches!(token.kind(), SyntaxKind::KwIn | SyntaxKind::LBrace)),
            "loop-binding recovery consumed the iterable or body boundary"
        );

        let for_statement = root
            .descendants()
            .find(|node| node.kind() == SyntaxKind::ForStatement)
            .expect("for statement remains structured");
        assert!(
            for_statement
                .descendants_with_tokens()
                .filter_map(rowan::NodeOrToken::into_token)
                .any(|token| token.kind() == SyntaxKind::Ident && token.text() == "values")
        );
        assert!(
            for_statement
                .descendants()
                .any(|node| node.kind() == SyntaxKind::Body),
            "for body was not preserved"
        );
        assert_eq!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                .count(),
            2,
            "loop-binding recovery swallowed the later declaration"
        );
    }
}
