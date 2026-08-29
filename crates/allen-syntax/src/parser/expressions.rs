use super::Parser;
use crate::SyntaxKind;

impl Parser<'_, '_> {
    pub(super) fn expression(&mut self) {
        self.expression_with_range();
    }

    fn expression_with_range(&mut self) -> bool {
        let marker = self.start();
        if self.exceeded {
            return false;
        }
        self.expression_depth += 1;
        let has_range = self.range();
        self.expression_depth = self.expression_depth.saturating_sub(1);
        self.complete(marker, SyntaxKind::Expression);
        has_range
    }

    pub(super) fn expression_before_body(&mut self) {
        let previous = self.postfix_brace_boundary;
        self.postfix_brace_boundary = Some(self.expression_depth + 1);
        self.expression();
        self.postfix_brace_boundary = previous;
    }

    pub(super) fn at_expression_start(&self) -> bool {
        matches!(
            self.nth(0),
            SyntaxKind::Ident
                | SyntaxKind::IntLiteral
                | SyntaxKind::FloatLiteral
                | SyntaxKind::StringLiteral
                | SyntaxKind::MultilineStringDelimiter
                | SyntaxKind::BytesLiteral
                | SyntaxKind::KwTrue
                | SyntaxKind::KwFalse
                | SyntaxKind::KwNone
                | SyntaxKind::KwSome
                | SyntaxKind::KwOk
                | SyntaxKind::KwErr
                | SyntaxKind::LParen
                | SyntaxKind::LBrace
                | SyntaxKind::LBracket
                | SyntaxKind::KwMap
                | SyntaxKind::Backtick
                | SyntaxKind::Bang
                | SyntaxKind::Minus
                | SyntaxKind::KwAwait
                | SyntaxKind::KwSpawn
                | SyntaxKind::KwIf
                | SyntaxKind::KwMatch
                | SyntaxKind::KwPrompt
                | SyntaxKind::KwFn
                | SyntaxKind::ErrorToken
        )
    }

    fn range(&mut self) -> bool {
        let marker = self.start();
        if self.exceeded {
            return false;
        }
        self.coalescing();
        let has_range = matches!(self.nth(0), SyntaxKind::DotDot | SyntaxKind::DotDotEq);
        if has_range {
            self.bump_nontrivia();
            self.coalescing();
            if matches!(self.nth(0), SyntaxKind::DotDot | SyntaxKind::DotDotEq) {
                self.error("range operators are nonassociative; parenthesize the nested range");
            }
        }
        self.complete(marker, SyntaxKind::Range);
        has_range
    }

    fn coalescing(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.pipeline();
        if self.at(SyntaxKind::QuestionQuestion) {
            self.bump_nontrivia();
            self.coalescing();
        }
        self.complete(marker, SyntaxKind::Coalescing);
    }

    fn pipeline(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.composition();
        while self.at(SyntaxKind::PipeGt) {
            self.bump_nontrivia();
            self.composition();
        }
        self.complete(marker, SyntaxKind::Pipeline);
    }

    fn composition(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.disjunction();
        while self.at(SyntaxKind::Gt) && self.nth(1) == SyntaxKind::Gt {
            self.bump_nontrivia();
            self.bump_nontrivia();
            self.disjunction();
        }
        self.complete(marker, SyntaxKind::Composition);
    }

    fn disjunction(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.conjunction();
        while self.at(SyntaxKind::PipePipe) {
            self.bump_nontrivia();
            self.conjunction();
        }
        self.complete(marker, SyntaxKind::Disjunction);
    }

    fn conjunction(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.equality();
        while self.at(SyntaxKind::AmpAmp) {
            self.bump_nontrivia();
            self.equality();
        }
        self.complete(marker, SyntaxKind::Conjunction);
    }

    fn equality(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.comparison();
        while matches!(self.nth(0), SyntaxKind::EqEq | SyntaxKind::NotEq) {
            self.bump_nontrivia();
            self.comparison();
        }
        self.complete(marker, SyntaxKind::Equality);
    }

    fn comparison(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.addition();
        while matches!(
            self.nth(0),
            SyntaxKind::Lt | SyntaxKind::LtEq | SyntaxKind::Gt | SyntaxKind::GtEq
        ) && !(self.nth(0) == SyntaxKind::Gt && self.nth(1) == SyntaxKind::Gt)
        {
            self.bump_nontrivia();
            self.addition();
        }
        self.complete(marker, SyntaxKind::Comparison);
    }

    fn addition(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.multiplication();
        while matches!(self.nth(0), SyntaxKind::Plus | SyntaxKind::Minus) {
            self.bump_nontrivia();
            self.multiplication();
        }
        self.complete(marker, SyntaxKind::Addition);
    }

    fn multiplication(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.unary();
        while matches!(
            self.nth(0),
            SyntaxKind::Star | SyntaxKind::Slash | SyntaxKind::Percent
        ) {
            self.bump_nontrivia();
            self.unary();
        }
        self.complete(marker, SyntaxKind::Multiplication);
    }

    fn unary(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        match self.nth(0) {
            SyntaxKind::KwAwait if self.nth(1) == SyntaxKind::LBrace => self.postfix(false),
            SyntaxKind::Bang | SyntaxKind::KwAwait | SyntaxKind::KwSpawn => {
                self.bump_nontrivia();
                self.unary();
            }
            SyntaxKind::Minus => {
                self.bump_nontrivia();
                if self
                    .nth_text(0)
                    .is_some_and(crate::lexer::is_minimum_int_magnitude)
                {
                    let inner = self.start();
                    self.direct_min_magnitude_operand();
                    self.complete(inner, SyntaxKind::Unary);
                } else {
                    self.unary();
                }
            }
            _ => self.postfix(false),
        }
        self.complete(marker, SyntaxKind::Unary);
    }

    fn direct_min_magnitude_operand(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.primary(true);
        if self.at_postfix_operator() {
            self.error("minimum integer magnitude cannot have a postfix operator");
        }
        self.complete(marker, SyntaxKind::Postfix);
    }

    fn postfix(&mut self, allow_min_magnitude: bool) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.primary(allow_min_magnitude);
        while !self.exceeded {
            match self.nth(0) {
                SyntaxKind::LBracket => self.index_postfix(),
                SyntaxKind::Dot if matches!(self.nth(1), SyntaxKind::Ident | SyntaxKind::KwMap) => {
                    self.bump_nontrivia();
                    self.bump_nontrivia();
                }
                SyntaxKind::QuestionDot
                    if matches!(self.nth(1), SyntaxKind::Ident | SyntaxKind::KwMap) =>
                {
                    self.bump_nontrivia();
                    self.bump_nontrivia();
                }
                SyntaxKind::LParen => self.call_postfix(),
                SyntaxKind::Lt if self.at_allowed_type_argument_call() => {
                    self.type_argument();
                    self.call_postfix();
                }
                SyntaxKind::Question => self.bump_nontrivia(),
                _ => break,
            }
        }
        self.complete(marker, SyntaxKind::Postfix);
    }

    fn at_postfix_operator(&self) -> bool {
        matches!(
            self.nth(0),
            SyntaxKind::LBracket
                | SyntaxKind::Dot
                | SyntaxKind::QuestionDot
                | SyntaxKind::LParen
                | SyntaxKind::Question
        ) || (self.nth(0) == SyntaxKind::Lt && self.at_allowed_type_argument_call())
    }

    fn primary(&mut self, allow_min_magnitude: bool) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        match self.nth(0) {
            SyntaxKind::Ident if self.nth_text(0) == Some("None") => {
                self.literal(allow_min_magnitude);
            }
            SyntaxKind::IntLiteral
            | SyntaxKind::FloatLiteral
            | SyntaxKind::StringLiteral
            | SyntaxKind::BytesLiteral
            | SyntaxKind::KwTrue
            | SyntaxKind::KwFalse
            | SyntaxKind::KwNone => self.literal(allow_min_magnitude),
            SyntaxKind::KwMap if self.nth(1) == SyntaxKind::LBrace => self.map_literal(),
            SyntaxKind::KwAwait if self.nth(1) == SyntaxKind::LBrace => self.await_block(),
            SyntaxKind::KwMap if self.nth(1) == SyntaxKind::Dot => self.bump_nontrivia(),
            SyntaxKind::Ident if self.at_contextual_constructor_call() => {
                self.bump_contextual_constructor();
            }
            SyntaxKind::KwSome | SyntaxKind::KwOk | SyntaxKind::KwErr | SyntaxKind::ErrorToken => {
                self.bump_nontrivia();
            }
            SyntaxKind::Ident => self.identifier_primary(),
            SyntaxKind::LParen if self.nth(1) == SyntaxKind::RParen => {
                self.literal(allow_min_magnitude);
            }
            SyntaxKind::LParen => self.tuple_or_group(),
            SyntaxKind::LBrace => self.anonymous_record(),
            SyntaxKind::LBracket => self.list_literal(),
            SyntaxKind::KwMap => self.error_one("expected `.` or `{` after `map`"),
            SyntaxKind::Backtick | SyntaxKind::MultilineStringDelimiter => self.template_literal(),
            SyntaxKind::KwIf => self.conditional_expression(),
            SyntaxKind::KwMatch => self.match_expression(),
            SyntaxKind::KwPrompt => self.prompt_expression(),
            SyntaxKind::KwFn => self.closure_expression(),
            _ => self.missing("expected expression"),
        }
        self.complete(marker, SyntaxKind::Primary);
    }

    pub(super) fn literal(&mut self, allow_min_magnitude: bool) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        match self.nth(0) {
            SyntaxKind::IntLiteral => {
                self.int_literal(allow_min_magnitude, "expected integer literal");
            }
            SyntaxKind::Ident if self.nth_text(0) == Some("None") => {
                self.eat_trivia();
                self.token_as(1, Some(SyntaxKind::KwNone));
            }
            SyntaxKind::FloatLiteral
            | SyntaxKind::StringLiteral
            | SyntaxKind::BytesLiteral
            | SyntaxKind::KwTrue
            | SyntaxKind::KwFalse
            | SyntaxKind::KwNone => self.bump_nontrivia(),
            SyntaxKind::LParen => {
                if self
                    .expect_open_delimiter(SyntaxKind::LParen, "expected `(` before unit literal")
                {
                    self.expect_close_delimiter(
                        SyntaxKind::RParen,
                        "expected `)` after unit literal",
                    );
                }
            }
            _ => self.missing("expected literal"),
        }
        self.complete(marker, SyntaxKind::Literal);
    }

    fn int_literal(&mut self, allow_min_magnitude: bool, missing_message: &'static str) {
        if self.nth(0) != SyntaxKind::IntLiteral {
            self.missing(missing_message);
            return;
        }
        if let Some(text) = self.nth_text(0) {
            if !crate::lexer::int_magnitude_supported(text) {
                self.error("integer literal exceeds Int range");
            } else if crate::lexer::is_minimum_int_magnitude(text) && !allow_min_magnitude {
                self.error("integer literal magnitude requires unary `-`");
            }
        }
        self.bump_nontrivia();
    }

    fn identifier_primary(&mut self) {
        if self.nth(1) == SyntaxKind::Dot
            && self.nth(2) == SyntaxKind::Ident
            && self.nth(3) == SyntaxKind::LBrace
            && !self.at_postfix_brace_boundary()
        {
            self.enum_record_constructor();
        } else if self.nth(1) == SyntaxKind::LBrace && !self.at_postfix_brace_boundary() {
            self.record_constructor();
        } else if self.nth(1) == SyntaxKind::Dot && self.nth(2) == SyntaxKind::Ident {
            self.qualified_enum();
        } else {
            self.bump_nontrivia();
        }
    }

    fn qualified_enum(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.expect(SyntaxKind::Ident, "expected enum type name");
        self.expect(SyntaxKind::Dot, "expected `.` before enum variant");
        self.expect(SyntaxKind::Ident, "expected enum variant name");
        self.complete(marker, SyntaxKind::QualifiedEnum);
    }

    fn at_postfix_brace_boundary(&self) -> bool {
        self.postfix_brace_boundary == Some(self.expression_depth)
    }

    fn at_contextual_constructor_call(&self) -> bool {
        matches!(self.nth_text(0), Some("Some" | "Ok" | "Err")) && self.nth(1) == SyntaxKind::LParen
    }

    fn bump_contextual_constructor(&mut self) {
        let kind = match self.nth_text(0) {
            Some("Some") => SyntaxKind::KwSome,
            Some("Ok") => SyntaxKind::KwOk,
            Some("Err") => SyntaxKind::KwErr,
            _ => SyntaxKind::Ident,
        };
        self.eat_trivia();
        self.token_as(1, Some(kind));
    }

    fn enum_record_constructor(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.expect(SyntaxKind::Ident, "expected enum type name");
        self.expect(SyntaxKind::Dot, "expected `.` before enum variant");
        self.expect(SyntaxKind::Ident, "expected enum variant name");
        if !self.expect_open_delimiter(
            SyntaxKind::LBrace,
            "expected `{` before enum record constructor",
        ) {
            self.complete(marker, SyntaxKind::EnumRecordConstructor);
            return;
        }
        self.record_value_fields();
        self.expect_close_delimiter(
            SyntaxKind::RBrace,
            "expected `}` after enum record constructor",
        );
        self.complete(marker, SyntaxKind::EnumRecordConstructor);
    }

    fn record_constructor(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.expect(SyntaxKind::Ident, "expected record type name");
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` before record constructor")
        {
            self.complete(marker, SyntaxKind::RecordConstructor);
            return;
        }
        self.record_constructor_contents();
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after record constructor");
        self.complete(marker, SyntaxKind::RecordConstructor);
    }

    fn anonymous_record(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` before anonymous record") {
            self.complete(marker, SyntaxKind::AnonymousRecord);
            return;
        }
        self.record_constructor_contents();
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after anonymous record");
        self.complete(marker, SyntaxKind::AnonymousRecord);
    }

    fn record_constructor_contents(&mut self) {
        if self.at(SyntaxKind::DotDot) {
            self.record_update_base();
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
            } else if !self.at(SyntaxKind::RBrace) {
                self.missing("expected `,` after record update base");
            }
        }
        self.record_value_fields();
    }

    fn record_update_base(&mut self) {
        let marker = self.start();
        self.expect(
            SyntaxKind::DotDot,
            "expected `..` before record update base",
        );
        self.expression();
        self.complete(marker, SyntaxKind::RecordUpdateBase);
    }

    fn record_value_fields(&mut self) {
        while !self.exceeded && !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            if self.at(SyntaxKind::Ident) {
                self.record_value_field();
            } else {
                self.error_one("expected record value field");
            }
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
            } else if !self.at(SyntaxKind::Ident)
                && !self.at(SyntaxKind::RBrace)
                && !self.at(SyntaxKind::Eof)
            {
                self.error_one("expected `,` or another record value field");
            }
        }
    }

    fn record_value_field(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.expect(SyntaxKind::Ident, "expected record value field name");
        if self.at(SyntaxKind::Colon) {
            self.bump_nontrivia();
            self.expression();
        }
        self.complete(marker, SyntaxKind::RecordValueField);
    }

    fn list_literal(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        if !self.expect_open_delimiter(SyntaxKind::LBracket, "expected `[` before list literal") {
            self.complete(marker, SyntaxKind::ListLiteral);
            return;
        }
        let mut expect_item = !self.at(SyntaxKind::RBracket);
        while !self.exceeded && !self.at_expression_list_end(SyntaxKind::RBracket) {
            if self.at(SyntaxKind::Comma) {
                if expect_item {
                    self.error_one("expected list item before `,`");
                } else {
                    self.bump_nontrivia();
                    expect_item = !self.at_expression_list_end(SyntaxKind::RBracket);
                }
                continue;
            }
            if !self.at(SyntaxKind::DotDot) && !self.at_expression_start() {
                self.error_one("expected list item");
                expect_item = false;
                continue;
            }
            if !expect_item {
                self.missing("expected `,` between list items");
            }
            self.list_item();
            expect_item = false;
        }
        self.expect_close_delimiter(SyntaxKind::RBracket, "expected `]` after list literal");
        self.complete(marker, SyntaxKind::ListLiteral);
    }

    fn list_item(&mut self) {
        let marker = self.start();
        if self.at(SyntaxKind::DotDot) {
            self.bump_nontrivia();
        }
        self.expression();
        self.complete(marker, SyntaxKind::ListItem);
    }

    fn map_literal(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.expect(SyntaxKind::KwMap, "expected `map` before map literal");
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` before map literal") {
            self.complete(marker, SyntaxKind::MapLiteral);
            return;
        }
        let mut expect_entry = !self.at(SyntaxKind::RBrace);
        while !self.exceeded && !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            if self.at(SyntaxKind::Comma) {
                if expect_entry {
                    self.error_one("expected map entry before `,`");
                } else {
                    self.bump_nontrivia();
                    expect_entry = !self.at(SyntaxKind::RBrace);
                }
                continue;
            }
            if self.at(SyntaxKind::DotDot) || self.at_map_entry_start() {
                self.map_item();
                expect_entry = false;
            } else {
                self.error_one("expected map entry");
                expect_entry = false;
            }
        }
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after map literal");
        self.complete(marker, SyntaxKind::MapLiteral);
    }

    fn at_map_entry_start(&self) -> bool {
        self.at_expression_start()
            && !matches!(
                self.nth(0),
                SyntaxKind::RBrace
                    | SyntaxKind::RBracket
                    | SyntaxKind::RParen
                    | SyntaxKind::Semi
                    | SyntaxKind::Comma
                    | SyntaxKind::Eof
            )
    }

    fn map_item(&mut self) {
        let marker = self.start();
        if self.at(SyntaxKind::DotDot) {
            self.bump_nontrivia();
            self.expression();
        } else {
            self.expression();
            self.expect(SyntaxKind::Colon, "expected `:` between map key and value");
            self.expression();
        }
        self.complete(marker, SyntaxKind::MapItem);
    }

    fn tuple_or_group(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        if !self.expect_open_delimiter(SyntaxKind::LParen, "expected `(` before tuple or group") {
            self.complete(marker, SyntaxKind::TupleOrGroup);
            return;
        }
        self.expression_list(SyntaxKind::RParen, "expected `)` after tuple or group");
        self.complete(marker, SyntaxKind::TupleOrGroup);
    }

    fn template_literal(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        let delimiter = match self.nth(0) {
            SyntaxKind::Backtick => SyntaxKind::Backtick,
            SyntaxKind::MultilineStringDelimiter => SyntaxKind::MultilineStringDelimiter,
            _ => {
                self.missing("expected template start");
                self.complete(marker, SyntaxKind::TemplateLiteral);
                return;
            }
        };
        if !self.expect_open_delimiter(delimiter, "expected template start") {
            self.complete(marker, SyntaxKind::TemplateLiteral);
            return;
        }
        while !self.exceeded && !self.at(delimiter) && !self.at(SyntaxKind::Eof) {
            match self.nth(0) {
                SyntaxKind::TemplateTextScalar | SyntaxKind::TemplateEscape => {
                    self.template_segment();
                }
                SyntaxKind::TemplateExprStart => self.template_interpolation(),
                _ => self.error_one("expected template segment or interpolation"),
            }
        }
        self.expect_close_delimiter(delimiter, "expected template end");
        self.complete(marker, SyntaxKind::TemplateLiteral);
    }

    fn template_segment(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        while matches!(
            self.nth(0),
            SyntaxKind::TemplateTextScalar | SyntaxKind::TemplateEscape
        ) {
            let part = self.start();
            if self.exceeded {
                return;
            }
            self.bump_nontrivia();
            self.complete(part, SyntaxKind::TemplateTextOrEscape);
        }
        self.complete(marker, SyntaxKind::TemplateSegment);
    }

    fn template_interpolation(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        if !self.expect_open_delimiter(SyntaxKind::TemplateExprStart, "expected `${`") {
            self.complete(marker, SyntaxKind::TemplateInterpolation);
            return;
        }
        self.expression();
        self.expect_close_delimiter(
            SyntaxKind::RBrace,
            "expected `}` after template interpolation",
        );
        self.complete(marker, SyntaxKind::TemplateInterpolation);
    }

    fn await_block(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.expect(SyntaxKind::KwAwait, "expected `await`");
        self.body();
        self.complete(marker, SyntaxKind::AwaitBlock);
    }

    fn index_postfix(&mut self) {
        let marker = self.start();
        if !self.expect_open_delimiter(SyntaxKind::LBracket, "expected `[` before index expression")
        {
            return;
        }
        self.expression_depth += 1;
        self.expression();
        self.expression_depth = self.expression_depth.saturating_sub(1);
        self.expect_close_delimiter(SyntaxKind::RBracket, "expected `]` after index expression");
        self.complete(marker, SyntaxKind::Slice);
    }

    fn call_postfix(&mut self) {
        if !self.expect_open_delimiter(SyntaxKind::LParen, "expected `(` before call arguments") {
            return;
        }
        let mut expect_argument = !self.at(SyntaxKind::RParen);
        while !self.exceeded && !self.at_expression_list_end(SyntaxKind::RParen) {
            if self.at(SyntaxKind::Comma) {
                if expect_argument {
                    self.error_one("expected call argument before `,`");
                } else {
                    self.bump_nontrivia();
                    expect_argument = !self.at_expression_list_end(SyntaxKind::RParen);
                }
                continue;
            }
            if !self.at_expression_start() {
                self.error_one("expected call argument");
                expect_argument = false;
                continue;
            }
            if !expect_argument {
                self.missing("expected `,` between call arguments");
            }
            self.call_argument();
            expect_argument = false;
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
                expect_argument = !self.at_expression_list_end(SyntaxKind::RParen);
            } else if !self.at_expression_list_end(SyntaxKind::RParen) {
                self.missing("expected `,` between call arguments");
            }
        }
        self.expect_close_delimiter(SyntaxKind::RParen, "expected `)` after call arguments");
        if self.at(SyntaxKind::KwFn) {
            self.closure_expression();
        }
    }

    fn call_argument(&mut self) {
        let marker = self.start();
        if self.nth(0) == SyntaxKind::Ident && self.nth(1) == SyntaxKind::Colon {
            self.bump_nontrivia();
            self.expect(SyntaxKind::Colon, "expected `:` after call argument label");
        }
        if self.nth_text(0) == Some("_") {
            self.eat_trivia();
            self.token_as(1, Some(SyntaxKind::Underscore));
        } else {
            self.expression();
        }
        self.complete(marker, SyntaxKind::CallArgument);
    }

    fn expression_list(&mut self, close: SyntaxKind, close_message: &'static str) {
        while !self.exceeded && !self.at_expression_list_end(close) {
            if self.at(SyntaxKind::Comma) {
                self.error_one("expected expression before `,`");
                continue;
            }
            if self.at_expression_start() {
                self.expression();
            } else {
                self.error_one("expected expression in delimited list");
                continue;
            }
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
            } else if !self.at_expression_list_end(close) {
                self.missing("expected `,` between expressions");
            }
        }
        self.expect_close_delimiter(close, close_message);
    }

    fn at_expression_list_end(&self, close: SyntaxKind) -> bool {
        self.at(close)
            || self.at_match_arm_boundary()
            || matches!(
                self.nth(0),
                SyntaxKind::RParen
                    | SyntaxKind::RBracket
                    | SyntaxKind::RBrace
                    | SyntaxKind::Colon
                    | SyntaxKind::Semi
                    | SyntaxKind::Eof
            )
    }

    fn at_match_arm_boundary(&self) -> bool {
        if self.at(SyntaxKind::FatArrow) {
            return true;
        }
        if !self.at_pattern_start() {
            return false;
        }

        let remaining_depth = self
            .limits
            .delimiter_depth
            .saturating_sub(self.delimiter_depth);
        let mut closers = Vec::new();
        let mut cursor = self.pos;
        let mut scanned = 0usize;
        while let Some(token) = self.tokens.get(cursor) {
            let kind = token.kind();
            if super::is_trivia(kind) {
                cursor += 1;
                continue;
            }
            if closers.last().is_some_and(|expected| *expected == kind) {
                closers.pop();
            } else if kind == SyntaxKind::FatArrow && closers.is_empty() {
                return true;
            } else if closers.is_empty()
                && matches!(
                    kind,
                    SyntaxKind::Comma
                        | SyntaxKind::Colon
                        | SyntaxKind::RParen
                        | SyntaxKind::RBracket
                        | SyntaxKind::RBrace
                        | SyntaxKind::Semi
                        | SyntaxKind::Eof
                )
            {
                return false;
            } else if let Some(close) = match_arm_lookahead_closer(kind) {
                if closers.len() >= remaining_depth {
                    return false;
                }
                closers.push(close);
            } else if matches!(
                kind,
                SyntaxKind::RParen | SyntaxKind::RBracket | SyntaxKind::RBrace
            ) {
                return false;
            }
            scanned += 1;
            if scanned > self.limits.tokens {
                return false;
            }
            cursor += 1;
        }
        false
    }

    fn type_argument(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        if !self.expect_open_delimiter(SyntaxKind::Lt, "expected `<` before type argument") {
            self.complete(marker, SyntaxKind::TypeArgument);
            return;
        }
        self.type_syntax();
        self.expect_close_delimiter(SyntaxKind::Gt, "expected `>` after type argument");
        self.complete(marker, SyntaxKind::TypeArgument);
    }

    fn at_allowed_type_argument_call(&self) -> bool {
        self.at_allowed_type_argument_callee() && self.at_type_argument_call_shape()
    }

    fn at_allowed_type_argument_callee(&self) -> bool {
        let Some(last) = self.previous_nontrivia_text(0) else {
            return false;
        };
        if matches!(last, "narrow" | "decode") {
            return true;
        }
        self.previous_nontrivia_text(1) == Some(".")
            && matches!(
                (self.previous_nontrivia_text(2), last),
                (Some("agent" | "user"), "ask")
                    | (Some("model"), "request")
                    | (Some("sub_agent"), "run" | "ask")
            )
    }

    fn previous_nontrivia_text(&self, mut offset: usize) -> Option<&str> {
        let mut index = self.pos.checked_sub(1)?;
        loop {
            let token = self.tokens.get(index)?;
            if !super::is_trivia(token.kind()) {
                if offset == 0 {
                    return Some(token.text(self.source));
                }
                offset -= 1;
            }
            index = index.checked_sub(1)?;
        }
    }

    fn at_type_argument_call_shape(&self) -> bool {
        let remaining_depth = self
            .limits
            .delimiter_depth
            .saturating_sub(self.delimiter_depth);
        if remaining_depth == 0 {
            return false;
        }
        let mut closers = Vec::new();
        let Some(mut cursor) = self.next_nontrivia_raw(self.pos) else {
            return false;
        };
        let mut scanned = 0usize;
        while let Some(token) = self.tokens.get(cursor) {
            let kind = token.kind();
            if super::is_trivia(kind) {
                cursor += 1;
                continue;
            }
            if let Some(expected) = closers.last() {
                if *expected == kind {
                    closers.pop();
                    if closers.is_empty() {
                        let Some(next) = self.next_nontrivia_raw(cursor + 1) else {
                            return false;
                        };
                        return self.tokens[next].kind() == SyntaxKind::LParen;
                    }
                } else if let Some(nested) = type_argument_lookahead_closer(kind) {
                    if closers.len() >= remaining_depth {
                        return false;
                    }
                    closers.push(nested);
                } else if type_argument_lookahead_terminates(kind) {
                    return false;
                }
            } else if let Some(nested) = type_argument_lookahead_closer(kind) {
                if closers.len() >= remaining_depth {
                    return false;
                }
                closers.push(nested);
            } else {
                return false;
            }
            scanned += 1;
            if scanned > self.limits.tokens {
                return false;
            }
            cursor += 1;
        }
        false
    }

    fn next_nontrivia_raw(&self, mut index: usize) -> Option<usize> {
        while let Some(token) = self.tokens.get(index) {
            if !super::is_trivia(token.kind()) {
                return Some(index);
            }
            index += 1;
        }
        None
    }

    fn closure_expression(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.expect(SyntaxKind::KwFn, "expected `fn`");
        if self.at_short_closure() {
            self.short_closure_parameter_list();
            self.expect(
                SyntaxKind::FatArrow,
                "expected `=>` after concise lambda parameters",
            );
            self.expression();
            self.complete(marker, SyntaxKind::ShortClosure);
            return;
        }
        self.parameter_list();
        self.expect(
            SyntaxKind::KwReturns,
            "expected `returns` after closure parameters",
        );
        self.type_syntax();
        if self.at(SyntaxKind::KwEffects) {
            self.effect_clause();
        }
        if self.at(SyntaxKind::LBrace) {
            self.body();
        } else {
            self.missing("expected closure body");
        }
        self.complete(marker, SyntaxKind::Closure);
    }

    fn at_short_closure(&self) -> bool {
        if self.nth(0) != SyntaxKind::LParen {
            return false;
        }
        let mut index = self.pos;
        let mut depth = 0usize;
        while let Some(token) = self.tokens.get(index) {
            if super::is_trivia(token.kind()) {
                index += 1;
                continue;
            }
            match token.kind() {
                SyntaxKind::LParen => depth += 1,
                SyntaxKind::RParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return self
                            .next_nontrivia_raw(index + 1)
                            .is_some_and(|next| self.tokens[next].kind() == SyntaxKind::FatArrow);
                    }
                }
                SyntaxKind::Eof => return false,
                _ => {}
            }
            index += 1;
        }
        false
    }

    fn short_closure_parameter_list(&mut self) {
        if !self.expect_open_delimiter(SyntaxKind::LParen, "expected concise lambda parameter list")
        {
            return;
        }
        let mut expect_parameter = !self.at(SyntaxKind::RParen);
        while !self.exceeded && !self.at(SyntaxKind::RParen) && !self.at(SyntaxKind::Eof) {
            if self.at(SyntaxKind::Comma) {
                if expect_parameter {
                    self.error_one("expected concise lambda parameter before `,`");
                } else {
                    self.bump_nontrivia();
                    expect_parameter = !self.at(SyntaxKind::RParen);
                }
            } else if self.at(SyntaxKind::Ident) {
                if !expect_parameter {
                    self.missing("expected `,` between concise lambda parameters");
                }
                self.bump_nontrivia();
                expect_parameter = false;
            } else {
                self.error_one("expected concise lambda parameter");
                expect_parameter = false;
            }
        }
        self.expect_close_delimiter(
            SyntaxKind::RParen,
            "expected `)` after concise lambda parameters",
        );
    }

    pub(super) fn conditional_expression(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.expect(SyntaxKind::KwIf, "expected `if`");
        if self.expect_open_delimiter(SyntaxKind::LParen, "expected `(` after `if`") {
            self.expression();
            self.expect_close_delimiter(SyntaxKind::RParen, "expected `)` after if condition");
        }
        if self.at(SyntaxKind::LBrace) {
            self.body();
        } else {
            self.missing("expected if body");
        }
        if self.at(SyntaxKind::KwElse) {
            self.bump_nontrivia();
            if self.at(SyntaxKind::KwIf) {
                self.conditional_expression();
            } else if self.at(SyntaxKind::LBrace) {
                self.body();
            } else {
                self.missing("expected `if` or body after `else`");
            }
        }
        self.complete(marker, SyntaxKind::ConditionalExpression);
    }

    fn prompt_expression(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        self.expect(SyntaxKind::KwPrompt, "expected `prompt`");
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` after `prompt`") {
            self.complete(marker, SyntaxKind::PromptExpression);
            return;
        }
        let mut saw_field = false;
        let mut expect_field = true;
        while !self.exceeded && !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            if self.at(SyntaxKind::Comma) {
                if expect_field {
                    self.error_one("expected prompt field before `,`");
                } else {
                    self.bump_nontrivia();
                    expect_field = !self.at(SyntaxKind::RBrace);
                }
                continue;
            }
            if self.at_prompt_field() {
                self.prompt_field();
                saw_field = true;
                expect_field = false;
            } else {
                self.error_one("expected prompt field");
                expect_field = false;
            }
        }
        if !saw_field {
            self.missing("expected prompt field");
        }
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after prompt expression");
        self.complete(marker, SyntaxKind::PromptExpression);
    }

    fn at_prompt_field(&self) -> bool {
        matches!(
            self.nth_text(0),
            Some("system" | "context" | "data" | "output" | "policy")
        ) && self.nth(0) == SyntaxKind::Ident
    }

    fn prompt_field(&mut self) {
        let marker = self.start();
        if self.exceeded {
            return;
        }
        match self.nth_text(0) {
            Some("system") => self.prompt_expression_field(SyntaxKind::KwSystem),
            Some("context") => self.prompt_expression_field(SyntaxKind::KwContext),
            Some("data") => self.prompt_expression_field(SyntaxKind::KwData),
            Some("output") => self.prompt_output_field(),
            Some("policy") => self.prompt_policy_field(),
            _ => self.missing("expected prompt field"),
        }
        self.complete(marker, SyntaxKind::PromptField);
    }

    fn prompt_expression_field(&mut self, kind: SyntaxKind) {
        self.bump_prompt_contextual(kind);
        self.expect(SyntaxKind::Colon, "expected `:` after prompt field name");
        self.expression();
    }

    fn prompt_output_field(&mut self) {
        self.bump_prompt_contextual(SyntaxKind::KwOutput);
        self.expect(SyntaxKind::Colon, "expected `:` after prompt `output`");
        self.type_syntax();
    }

    fn prompt_policy_field(&mut self) {
        self.bump_prompt_contextual(SyntaxKind::KwPolicy);
        self.expect(SyntaxKind::Colon, "expected `:` after prompt `policy`");
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` after prompt `policy`") {
            return;
        }
        self.expect_prompt_contextual(
            "max_attempts",
            SyntaxKind::KwMaxAttempts,
            "expected `max_attempts` in prompt policy",
        );
        self.expect(
            SyntaxKind::Colon,
            "expected `:` after prompt policy `max_attempts`",
        );
        self.int_literal(
            false,
            "expected integer literal for prompt policy `max_attempts`",
        );
        if self.at(SyntaxKind::Comma) {
            self.bump_nontrivia();
        }
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after prompt policy");
    }

    fn expect_prompt_contextual(
        &mut self,
        spelling: &str,
        kind: SyntaxKind,
        message: &'static str,
    ) -> bool {
        if self.nth(0) == SyntaxKind::Ident && self.nth_text(0) == Some(spelling) {
            self.bump_prompt_contextual(kind);
            true
        } else {
            self.missing(message);
            false
        }
    }

    fn bump_prompt_contextual(&mut self, kind: SyntaxKind) {
        self.eat_trivia();
        self.token_as(1, Some(kind));
    }
}

fn type_argument_lookahead_closer(kind: SyntaxKind) -> Option<SyntaxKind> {
    match kind {
        SyntaxKind::Lt => Some(SyntaxKind::Gt),
        SyntaxKind::LParen => Some(SyntaxKind::RParen),
        SyntaxKind::LBracket => Some(SyntaxKind::RBracket),
        SyntaxKind::LBrace => Some(SyntaxKind::RBrace),
        _ => None,
    }
}

fn match_arm_lookahead_closer(kind: SyntaxKind) -> Option<SyntaxKind> {
    match kind {
        SyntaxKind::LParen => Some(SyntaxKind::RParen),
        SyntaxKind::LBracket => Some(SyntaxKind::RBracket),
        SyntaxKind::LBrace | SyntaxKind::TemplateExprStart => Some(SyntaxKind::RBrace),
        _ => None,
    }
}

fn type_argument_lookahead_terminates(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Eof
            | SyntaxKind::Semi
            | SyntaxKind::RBrace
            | SyntaxKind::RParen
            | SyntaxKind::RBracket
            | SyntaxKind::Gt
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AstNode, Body, SourceFile, SourceFileId, parse, parse_with_limits};

    fn source(text: &str) -> SourceFile {
        SourceFile::new(SourceFileId::new(31), text).unwrap()
    }

    fn node_kinds(text: &str) -> Vec<SyntaxKind> {
        parse(&source(text))
            .syntax()
            .descendants()
            .map(|node| node.kind())
            .collect()
    }

    #[test]
    fn parses_simple_statements_and_expression_tails_losslessly() {
        let text = "fn main(input: List<Int>) returns Int {
  let min: Int = -9223372036854775808;
  let first = map.get(input);
  mut total = input[0] + 2 * 3;
  total += narrow<Int>(input)?;
  return total;
}";
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
            SyntaxKind::Statement,
            SyntaxKind::Expression,
            SyntaxKind::Disjunction,
            SyntaxKind::Comparison,
            SyntaxKind::Addition,
            SyntaxKind::Multiplication,
            SyntaxKind::Unary,
            SyntaxKind::Postfix,
            SyntaxKind::TypeArgument,
        ] {
            assert!(kinds.contains(&kind), "missing {kind:?}");
        }
    }

    #[test]
    fn parses_literals_templates_and_composite_values_losslessly() {
        let text = "fn values() returns Void {
  let unit = ();
  let tuple = (1, \"two\",);
  let list = [true, false, None, Some(1), Ok(2), Err(3),];
  let map_value = map { \"a\": 1 \"b\": 2, };
  let reading = Reading.Named { label: `cpu ${unit}`, value: tuple, };
  { label: reading.label, tuple }
}";
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
            SyntaxKind::Literal,
            SyntaxKind::TemplateLiteral,
            SyntaxKind::TemplateSegment,
            SyntaxKind::TemplateInterpolation,
            SyntaxKind::ListLiteral,
            SyntaxKind::MapLiteral,
            SyntaxKind::TupleOrGroup,
            SyntaxKind::EnumRecordConstructor,
            SyntaxKind::AnonymousRecord,
            SyntaxKind::RecordValueField,
        ] {
            assert!(kinds.contains(&kind), "missing {kind:?}");
        }
    }

    #[test]
    fn missing_statement_terminators_preserve_later_statements_and_declarations() {
        let text = "fn bad() returns Void { let first = 1 mut second = 2; first = second return first break continue; } fn later() returns Void {}";
        let source = source(text);
        let parsed = parse(&source);
        assert!(parsed.has_errors());
        assert!(
            parsed.diagnostics().len() <= 6,
            "{:?}",
            parsed.diagnostics()
        );
        parsed.assert_round_trip(&source);

        let kinds = node_kinds(text);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::Statement)
                .count(),
            6,
            "each following statement must remain independently recoverable"
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::FunctionDeclaration)
                .count(),
            2,
            "statement recovery must reach the later declaration"
        );
    }

    #[test]
    fn delimited_expression_lists_recover_missing_separators_at_their_own_closer() {
        for (text, list_kind, close) in [
            (
                "fn bad() returns Void { let value = [1 2, 3]; return; } fn later() returns Void {}",
                SyntaxKind::ListLiteral,
                SyntaxKind::RBracket,
            ),
            (
                "fn bad() returns Void { let value = call(1 2, 3); return; } fn later() returns Void {}",
                SyntaxKind::Postfix,
                SyntaxKind::RParen,
            ),
            (
                "fn bad() returns Void { let value = (1 2, 3); return; } fn later() returns Void {}",
                SyntaxKind::TupleOrGroup,
                SyntaxKind::RParen,
            ),
        ] {
            let source = source(text);
            let parsed = parse(&source);
            assert!(parsed.has_errors(), "accepted {text}");
            assert!(
                parsed.diagnostics().len() <= 4,
                "{:?}",
                parsed.diagnostics()
            );
            parsed.assert_round_trip(&source);

            let root = parsed.syntax();
            let list = root
                .descendants()
                .find(|node| node.kind() == list_kind)
                .expect("malformed list remains structured");
            assert!(
                list.descendants_with_tokens()
                    .filter_map(rowan::NodeOrToken::into_token)
                    .any(|token| token.kind() == close),
                "{list_kind:?} lost its {close:?}"
            );
            assert!(
                root.descendants()
                    .filter(|node| node.kind() == SyntaxKind::Statement)
                    .count()
                    >= 2,
                "list recovery swallowed the following return for {text}"
            );
            assert_eq!(
                root.descendants()
                    .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                    .count(),
                2,
                "list recovery did not reach the later declaration for {text}"
            );
        }
    }

    #[test]
    fn delimited_expression_lists_preserve_foreign_enclosing_closers() {
        for (text, inner_kind, outer_kind, foreign_closer) in [
            (
                "fn bad() returns Void { let value = [call(1, 2]; return; } fn later() returns Void {}",
                SyntaxKind::Postfix,
                SyntaxKind::ListLiteral,
                SyntaxKind::RBracket,
            ),
            (
                "fn bad() returns Void { let value = call([1, 2); return; } fn later() returns Void {}",
                SyntaxKind::ListLiteral,
                SyntaxKind::Postfix,
                SyntaxKind::RParen,
            ),
        ] {
            let source = source(text);
            let parsed = parse(&source);
            assert!(parsed.has_errors(), "accepted {text}");
            parsed.assert_round_trip(&source);

            let root = parsed.syntax();
            let inner = root
                .descendants()
                .find(|node| {
                    node.kind() == inner_kind
                        && (inner_kind != SyntaxKind::Postfix
                            || node
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
                                .is_some_and(|token| token.text() == "call"))
                })
                .expect("inner malformed list remains structured");
            assert!(
                !inner
                    .descendants_with_tokens()
                    .filter_map(rowan::NodeOrToken::into_token)
                    .any(|token| token.kind() == foreign_closer),
                "inner {inner_kind:?} consumed foreign {foreign_closer:?}"
            );
            let outer = root
                .descendants()
                .find(|node| {
                    node.kind() == outer_kind
                        && (outer_kind != SyntaxKind::Postfix
                            || node
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
                                .is_some_and(|token| token.text() == "call"))
                })
                .expect("enclosing list remains structured");
            assert!(
                outer
                    .descendants_with_tokens()
                    .filter_map(rowan::NodeOrToken::into_token)
                    .any(|token| token.kind() == foreign_closer),
                "enclosing {outer_kind:?} lost {foreign_closer:?}"
            );
            assert!(
                root.descendants()
                    .filter(|node| node.kind() == SyntaxKind::Statement)
                    .count()
                    >= 2
            );
            assert_eq!(
                root.descendants()
                    .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                    .count(),
                2
            );
        }
    }

    #[test]
    fn unclosed_call_in_map_key_preserves_the_entry_colon_value_and_following_syntax() {
        let text = "fn bad() returns Void { let value = map { call(1: 2, \"ok\": 3 }; return; } fn later() returns Void {}";
        let source = source(text);
        let parsed = parse(&source);
        assert!(parsed.has_errors());
        parsed.assert_round_trip(&source);

        let root = parsed.syntax();
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
                .any(|token| token.kind() == SyntaxKind::Colon),
            "call recovery consumed the map-entry colon"
        );

        let map = root
            .descendants()
            .find(|node| node.kind() == SyntaxKind::MapLiteral)
            .expect("map literal remains structured");
        assert_eq!(
            map.descendants_with_tokens()
                .filter_map(rowan::NodeOrToken::into_token)
                .filter(|token| token.kind() == SyntaxKind::Colon)
                .count(),
            2,
            "map literal lost an entry colon"
        );
        assert!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::Statement)
                .count()
                >= 2,
            "map recovery swallowed the following return"
        );
        assert_eq!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                .count(),
            2,
            "map recovery swallowed the later declaration"
        );
    }

    #[test]
    fn malformed_template_interpolations_preserve_template_and_body_boundaries() {
        let text = "fn bad() returns Void { let text = `head ${} middle ${call(1 2)} tail`; return; } fn later() returns Void {}";
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
        let interpolations: Vec<_> = root
            .descendants()
            .filter(|node| node.kind() == SyntaxKind::TemplateInterpolation)
            .collect();
        assert_eq!(interpolations.len(), 2);
        assert!(
            interpolations.iter().all(|node| node
                .last_token()
                .is_some_and(|token| token.kind() == SyntaxKind::RBrace)),
            "each interpolation must retain its own closing brace"
        );
        let template = root
            .descendants()
            .find(|node| node.kind() == SyntaxKind::TemplateLiteral)
            .expect("template remains structured");
        assert_eq!(
            template.last_token().map(|token| token.kind()),
            Some(SyntaxKind::Backtick)
        );
        assert!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::Statement)
                .count()
                >= 2,
            "template recovery swallowed the following return"
        );
        assert_eq!(
            root.descendants()
                .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                .count(),
            2
        );
    }

    #[test]
    fn statement_list_and_template_token_boundary_truncations_round_trip() {
        let text = "// π\nfn f() returns Void { let values = [call(1, (2, 3,),),]; let message = `value ${{ item: values[0] }.item}`; return; }";
        let full = source(text);
        let token_starts: Vec<_> = crate::lex(&full)
            .tokens()
            .iter()
            .map(|token| u32::from(token.range().start()) as usize)
            .collect();
        for end in token_starts {
            let prefix = source(&text[..end]);
            let parsed = parse(&prefix);
            assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
            assert!(parsed.diagnostics().len() <= crate::SyntaxLimits::DEFAULT.diagnostics);
            parsed.assert_round_trip(&prefix);
        }
    }

    #[test]
    fn boundary_fixture_obeys_every_injected_syntax_counter() {
        let text = "fn f() returns Void { let values = [call(1, (2, 3,),),]; let message = `value ${{ item: values[0] }.item}`; return; }";
        let file = source(text);
        let source_limit = text.len() - 1;
        for limits in [
            crate::SyntaxLimits {
                source_bytes: source_limit,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                tokens: 1,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                lexer_mode_depth: 1,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                interpolation_brace_depth: 0,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                parser_recursion: 4,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                delimiter_depth: 1,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                events: 8,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                nodes: 4,
                ..crate::SyntaxLimits::DEFAULT
            },
        ] {
            let parsed = parse_with_limits(&file, limits);
            assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
            assert!(parsed.has_errors(), "limit did not trigger: {limits:?}");
            parsed.assert_round_trip(&file);
        }

        let malformed = source("fn f() returns Void { let values = [1 2]; return; }");
        let parsed = parse_with_limits(
            &malformed,
            crate::SyntaxLimits {
                diagnostics: 0,
                ..crate::SyntaxLimits::DEFAULT
            },
        );
        assert!(parsed.diagnostics().is_empty());
        assert!(parsed.has_errors());
        parsed.assert_round_trip(&malformed);
    }

    #[test]
    fn validates_contextual_int_min_magnitude_syntax() {
        for text in [
            "fn f() returns Int { -9223372036854775808 }",
            "fn f() returns Int { -9_223_372_036_854_775_808 }",
            "fn f() returns Int { --9223372036854775808 }",
            "fn f() returns Int { --1 }",
        ] {
            let valid = source(text);
            let parsed = parse(&valid);
            assert!(
                parsed.diagnostics().is_empty(),
                "{:?}",
                parsed.diagnostics()
            );
            assert!(!parsed.has_errors(), "rejected {text}");
            parsed.assert_round_trip(&valid);
        }

        for text in [
            "fn f() returns Int { 9223372036854775808 }",
            "fn f() returns Int { 9_223_372_036_854_775_808 }",
            "fn f() returns Int { 9223372036854775809 }",
            "fn f() returns Int { -(9223372036854775808) }",
            "fn f() returns Int { -9223372036854775808[0] }",
            "fn f() returns Int { -9223372036854775808() }",
        ] {
            let invalid = source(text);
            let parsed = parse(&invalid);
            assert!(parsed.has_errors(), "accepted {text}");
            parsed.assert_round_trip(&invalid);
        }
    }

    #[test]
    fn type_argument_lookahead_does_not_capture_comparisons() {
        let text = "fn f(a: Int, b: Int, c: Int) returns Bool { a < b > (c) }";
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
        assert!(kinds.contains(&SyntaxKind::Comparison));
        assert!(!kinds.contains(&SyntaxKind::TypeArgument));
    }

    #[test]
    fn conditional_statement_and_tail_expression_are_classified_by_position() {
        let statement = source("fn f(x: Bool) returns Void { if (x) { return; } return; }");
        let parsed = parse(&statement);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&statement);

        let unit_tail = source("fn f(x: Bool) returns Void { if (x) { return; } () }");
        let parsed = parse(&unit_tail);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        let body = parsed
            .syntax()
            .descendants()
            .find_map(Body::cast)
            .expect("outer function body");
        assert_eq!(body.statements().count(), 1);
        assert!(body.expression().is_some());
        parsed.assert_round_trip(&unit_tail);

        for (operator, right, expected_kind) in [
            ("==", "()", SyntaxKind::Equality),
            ("!=", "()", SyntaxKind::Equality),
            ("||", "true", SyntaxKind::Disjunction),
            ("&&", "true", SyntaxKind::Conjunction),
            ("<", "1", SyntaxKind::Comparison),
            ("+", "1", SyntaxKind::Addition),
            ("*", "1", SyntaxKind::Multiplication),
        ] {
            let text = format!("fn f() returns Void {{ if (true) {{ () }} {operator} {right} }}");
            let source = source(&text);
            let parsed = parse(&source);
            assert!(
                parsed.diagnostics().is_empty(),
                "{operator}: {:?}",
                parsed.diagnostics()
            );
            let body = parsed
                .syntax()
                .descendants()
                .find_map(Body::cast)
                .expect("outer function body");
            assert_eq!(body.statements().count(), 0, "{operator}");
            assert!(body.expression().is_some(), "{operator}");
            assert!(node_kinds(&text).contains(&expected_kind), "{operator}");
            parsed.assert_round_trip(&source);
        }

        let tail =
            source("fn f(x: Bool) returns Int { if (x) { 1 } else if (x) { 2 } else { 3 } }");
        let parsed = parse(&tail);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&tail);
        assert!(node_kinds(tail.text()).contains(&SyntaxKind::ConditionalExpression));
    }

    #[test]
    fn conditional_statement_lookahead_skips_large_body_and_template_interpolation() {
        let repeated = "let value = 1;\n".repeat(200);
        let text = format!(
            "fn f(x: Bool) returns Void {{ if (x) {{ let label = `value ${{if (x) {{ 1 }} else {{ 2 }}}}`;\n{repeated} }} return; }}"
        );
        let source = source(&text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);
        assert!(node_kinds(&text).contains(&SyntaxKind::ConditionalExpression));
    }

    #[test]
    fn conditional_expression_continuations_are_not_statement_boundaries() {
        for text in [
            "fn f(x: Bool) returns Int { if (x) { 1 } else { 2 } + 3 }",
            "fn f(x: Bool) returns Option<Int> { if (x) { Some(1) } else { None }? }",
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
        }
    }

    #[test]
    fn for_match_and_closure_forms_handle_nested_header_delimiters() {
        for text in [
            "fn f() returns Void { for item in map { \"a\": [`x ${1}`] } { return; } }",
            "fn f(value: Int) returns Int { match choose({ nested: [value] }) { _ => 1 } }",
            "fn f() returns Void { let c = fn(callback: fn({ a: Int }) returns Int) returns Int { 1 }; }",
            "fn f(users: List<Int>) returns List<Int> { list.filter(values: users, callback: fn(user) => user > 0) }",
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
        }
    }

    #[test]
    fn deep_type_argument_lookahead_uses_syntax_limits_not_a_small_constant() {
        let ty = format!("{}Int{}", "List<".repeat(40), ">".repeat(40));
        let text = format!("fn f(value: {ty}) returns Void {{ narrow<{ty}>(value) }}");
        let source = source(&text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);
        assert!(node_kinds(&text).contains(&SyntaxKind::TypeArgument));
    }

    #[test]
    fn decode_accepts_one_explicit_type_argument() {
        let text = "fn f() returns Result<Int, DecodeError> { decode<Int>(b\"1\") }";
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(node_kinds(text).contains(&SyntaxKind::TypeArgument));
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn wide_type_argument_lookahead_uses_token_limit_not_delimiter_width() {
        let params = (0..600).map(|_| "Int").collect::<Vec<_>>().join(", ");
        let ty = format!("fn({params}) returns Int");
        let text = format!("fn f(value: Int) returns Void {{ narrow<{ty}>(value) }}");
        let source = source(&text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);
        assert!(node_kinds(&text).contains(&SyntaxKind::TypeArgument));
    }

    #[test]
    fn injected_delimiter_depth_is_global_across_exact_productions() {
        for text in [
            "fn f() returns List<List<Int>> {}",
            "fn f() returns Void { `value ${x}` }",
            "manifest { tools: { required: [{ name: \"x\", version: \"1\" }] } }",
            "fn f() returns Void { prompt { policy: { max_attempts: 1 } } }",
            "fn f() returns Void { narrow<Int>(1) }",
        ] {
            let source = source(text);
            let parsed = parse_with_limits(
                &source,
                crate::SyntaxLimits {
                    delimiter_depth: 1,
                    ..crate::SyntaxLimits::DEFAULT
                },
            );
            assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
            assert!(parsed.has_errors(), "accepted {text}");
            parsed.assert_round_trip(&source);
        }
    }

    #[test]
    fn malformed_map_entry_consumes_bad_tokens_and_recovers_later_declarations() {
        let text = "fn f() returns Void { let bad = map { ] }; } fn later() returns Void {}";
        let source = source(text);
        let parsed = parse(&source);
        assert!(parsed.has_errors());
        assert!(
            parsed.diagnostics().len() <= 4,
            "{:?}",
            parsed.diagnostics()
        );
        parsed.assert_round_trip(&source);
        assert_eq!(
            node_kinds(text)
                .iter()
                .filter(|kind| **kind == SyntaxKind::FunctionDeclaration)
                .count(),
            2
        );
    }

    #[test]
    fn await_block_is_distinct_from_unary_await_operand() {
        let block = source("fn f() returns Void { await { return; } }");
        let parsed = parse(&block);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&block);
        assert!(node_kinds(block.text()).contains(&SyntaxKind::AwaitBlock));

        let unary = source("fn f(task: Task<Int>) returns Int { await task }");
        let parsed = parse(&unary);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&unary);
        assert!(!node_kinds(unary.text()).contains(&SyntaxKind::AwaitBlock));
    }

    #[test]
    fn none_is_contextualized_as_a_literal_in_expressions() {
        let text = "fn f() returns Option<Int> { None }";
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);
        assert!(node_kinds(text).contains(&SyntaxKind::Literal));

        let tokens: Vec<_> = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .map(|token| (token.kind(), token.text().to_owned()))
            .collect();
        assert!(tokens.contains(&(SyntaxKind::KwNone, "None".to_owned())));
    }

    #[test]
    fn option_and_result_constructors_are_contextual_only_when_called() {
        let text = "fn f() returns Void { let Some = 1; [Some(1), Ok(2), Err(3), Some] }";
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);

        let tokens: Vec<_> = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .map(|token| (token.kind(), token.text().to_owned()))
            .collect();
        assert!(tokens.contains(&(SyntaxKind::KwSome, "Some".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::KwOk, "Ok".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::KwErr, "Err".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::Ident, "Some".to_owned())));
    }

    #[test]
    fn qualified_enum_primary_is_mapped_without_breaking_postfix_or_constructors() {
        let text = "fn f(value: Reading) returns Void { let unit = Reading.Empty; let tuple = Reading.Number(1); let record_value = Reading.Named { value: 1 }; let field = value.field.more; match Reading.Empty { Reading.Empty => 0 } }";
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
        assert!(kinds.contains(&SyntaxKind::QualifiedEnum));
        assert!(kinds.contains(&SyntaxKind::EnumRecordConstructor));
        assert!(kinds.contains(&SyntaxKind::Postfix));
        assert!(kinds.contains(&SyntaxKind::MatchExpression));
        assert!(kinds.contains(&SyntaxKind::MatchArm));
    }

    #[test]
    fn builtin_namespace_map_member_accepts_the_map_keyword() {
        let text = "fn f(values: List<Int>) returns List<Int> { list.map(values, fn(item: Int) returns Int { item }) }";
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);

        assert!(
            parsed
                .syntax()
                .descendants_with_tokens()
                .filter_map(rowan::NodeOrToken::into_token)
                .any(|token| token.kind() == SyntaxKind::KwMap && token.text() == "map")
        );
    }

    #[test]
    fn prompt_fields_are_exact_nodes_and_contextual_tokens() {
        let text = r#"fn f() returns Prompt<Result<Int, String>> {
  let system = 1;
  prompt {
    system: "review",
    context: { user: "ada" },
    data: [1, 2],
    output: Result<Int, String>,
    policy: { max_attempts: 3, },
  }
}"#;
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
        assert!(kinds.contains(&SyntaxKind::PromptExpression));
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::PromptField)
                .count(),
            5
        );

        let tokens: Vec<_> = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .map(|token| (token.kind(), token.text().to_owned()))
            .collect();
        assert!(tokens.contains(&(SyntaxKind::KwSystem, "system".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::KwContext, "context".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::KwData, "data".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::KwOutput, "output".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::KwPolicy, "policy".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::KwMaxAttempts, "max_attempts".to_owned())));
        assert!(tokens.contains(&(SyntaxKind::Ident, "system".to_owned())));
    }

    #[test]
    fn prompt_policy_integer_uses_ordinary_magnitude_rules_without_bounding_effect_majors() {
        for (magnitude, expected) in [
            (
                "9223372036854775808",
                "integer literal magnitude requires unary `-`",
            ),
            ("9223372036854775809", "integer literal exceeds Int range"),
        ] {
            let text = format!(
                "fn f() returns Void {{ prompt {{ policy: {{ max_attempts: {magnitude} }} }} }}"
            );
            let source = source(&text);
            let parsed = parse(&source);
            assert!(parsed.has_errors(), "accepted {magnitude}");
            assert!(
                parsed
                    .diagnostics()
                    .iter()
                    .any(|diagnostic| diagnostic.message() == expected),
                "{:?}",
                parsed.diagnostics()
            );
            parsed.assert_round_trip(&source);
        }

        let major = "999999999999999999999999999999999999999999999999";
        let effect = format!("tool.release@{major}");
        let text = format!("fn f() returns Void effects [{effect}] {{}}");
        let source = source(&text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        assert!(
            parsed
                .syntax()
                .descendants_with_tokens()
                .filter_map(rowan::NodeOrToken::into_token)
                .any(|token| token.kind() == SyntaxKind::EffectId && token.text() == effect)
        );
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn map_and_prompt_reject_leading_or_consecutive_commas() {
        for text in [
            "fn f() returns Void { map { , \"a\": 1 } }",
            "fn f() returns Void { map { \"a\": 1,, \"b\": 2 } }",
            "fn f() returns Void { prompt { , system: \"x\", output: Int } }",
            "fn f() returns Void { prompt { system: \"x\",, output: Int } }",
        ] {
            let source = source(text);
            let parsed = parse(&source);
            assert!(parsed.has_errors(), "accepted {text}");
            parsed.assert_round_trip(&source);
        }

        for text in [
            "fn f() returns Void { map { \"a\": 1 \"b\": 2, } }",
            "fn f() returns Void { prompt { system: \"x\" output: Int, } }",
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
        }
    }

    #[test]
    fn minimum_integer_literal_keeps_recursive_unary_shape() {
        for text in [
            "fn f() returns Int { -9223372036854775808 }",
            "fn f() returns Int { -0_9223372036854775808 }",
        ] {
            let source = source(text);
            let parsed = parse(&source);
            assert!(
                parsed.diagnostics().is_empty(),
                "{:?}",
                parsed.diagnostics()
            );
            assert!(!parsed.has_errors());
            parsed.assert_round_trip(&source);
            assert!(
                node_kinds(text)
                    .iter()
                    .filter(|kind| **kind == SyntaxKind::Unary)
                    .count()
                    >= 2
            );
        }
    }

    #[test]
    fn lexer_error_token_can_serve_as_a_recovered_expression_operand() {
        let text = "fn f() returns Void { let bad = \"oops\\q\"; return; }";
        let source = source(text);
        let parsed = parse(&source);
        let diagnostics: Vec<_> = parsed
            .diagnostics()
            .iter()
            .map(crate::SyntaxDiagnostic::code)
            .collect();
        assert_eq!(diagnostics, vec!["S0002"]);
        parsed.assert_round_trip(&source);

        let kinds = node_kinds(text);
        assert!(kinds.contains(&SyntaxKind::Expression));
        assert!(kinds.contains(&SyntaxKind::Error));
    }

    #[test]
    fn deferred_body_forms_are_lossless_and_bounded() {
        let text = "fn f() returns Void { while (ready) { if (x) { return; } } continue; }";
        let file = source(text);
        let parsed = parse(&file);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&file);
        assert!(node_kinds(text).contains(&SyntaxKind::Statement));

        let deep = source(&format!(
            "fn f() returns Void {{ {}0{} }}",
            "(".repeat(80),
            ")".repeat(80)
        ));
        let parsed = parse_with_limits(
            &deep,
            crate::SyntaxLimits {
                parser_recursion: 12,
                ..crate::SyntaxLimits::DEFAULT
            },
        );
        assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
        assert!(parsed.has_errors());
        parsed.assert_round_trip(&deep);

        let nested = source("fn f() returns Void { [[(1)]] }");
        let parsed = parse_with_limits(
            &nested,
            crate::SyntaxLimits {
                delimiter_depth: 2,
                ..crate::SyntaxLimits::DEFAULT
            },
        );
        assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
        assert!(parsed.has_errors());
        parsed.assert_round_trip(&nested);
    }

    #[test]
    fn ranges_and_slices_pin_precedence_nonassociativity_and_composition_tokens() {
        let text = "fn f(values: List<Int>) returns Int { let ranged = 1 + 2..3 ?? 4; let sliced = values[1..=4]; let indexed = values[0]; let composed = left >> right; ranged }";
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
                .filter(|kind| **kind == SyntaxKind::Range)
                .count(),
            7,
            "every expression has a range precedence wrapper"
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::Slice)
                .count(),
            2,
            "index and slice postfixes share one lossless bracket carrier"
        );
        assert!(kinds.contains(&SyntaxKind::Composition));
        let tokens = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .map(|token| token.kind())
            .collect::<Vec<_>>();
        assert!(tokens.contains(&SyntaxKind::DotDot));
        assert!(tokens.contains(&SyntaxKind::DotDotEq));
        let composition = parsed
            .syntax()
            .descendants()
            .find(|node| {
                node.kind() == SyntaxKind::Composition
                    && node.text().to_string().contains("left >> right")
            })
            .expect("composition node");
        assert_eq!(
            composition
                .descendants_with_tokens()
                .filter_map(rowan::NodeOrToken::into_token)
                .filter(|token| token.kind() == SyntaxKind::Gt)
                .count(),
            2,
            "composition stays contextual as two `>` tokens"
        );

        let malformed = source("fn bad() returns Int { 1..2..3 } fn later() returns Void {}");
        let parsed = parse(&malformed);
        assert!(parsed.has_errors());
        assert!(parsed.diagnostics().iter().any(|diagnostic| {
            diagnostic.message()
                == "range operators are nonassociative; parenthesize the nested range"
        }));
        assert_eq!(
            parsed
                .syntax()
                .descendants()
                .filter(|node| node.kind() == SyntaxKind::FunctionDeclaration)
                .count(),
            2,
            "range recovery reaches the following declaration"
        );
        parsed.assert_round_trip(&malformed);
    }
}
