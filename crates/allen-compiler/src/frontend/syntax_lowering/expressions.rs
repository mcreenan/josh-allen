use super::{
    Binary, LowerResult, LoweredCallArgument, LoweredElse, LoweredEnumValuePayload, LoweredExpr,
    LoweredExprKind, LoweredListItem, LoweredMapItem, LoweredPattern, LoweredTemplatePart,
    SourceWordContext, Span, SyntaxLoweringError, Unary, checked_ident, compiler_error,
    decode_bytes, decode_string, is_forbidden_source_word, malformed_node, required,
    required_ident, span_node, span_token,
};
use allen_bytecode::canonical_float_bits;
use allen_syntax::{
    Addition, AnonymousRecord, AstNode, AwaitBlock, CallArgument, Closure, Coalescing, Comparison,
    Composition, ConditionalExpression, Conjunction, Disjunction, EnumPattern,
    EnumRecordConstructor, Equality, Expression, ListItem, ListLiteral, Literal, MapItem,
    MapLiteral, MatchExpression, Multiplication, Pattern, PatternField, PatternOr, PatternPrimary,
    PatternRange, Pipeline, Postfix, Primary, PromptExpression, QualifiedEnum, Range,
    RecordConstructor, RecordPattern, RecordValueField, ShortClosure, Slice, SyntaxKind,
    SyntaxNode, SyntaxToken, TemplateInterpolation, TemplateLiteral, TemplateSegment, TupleOrGroup,
    TypeArgument, Unary as SyntaxUnary,
};
use std::collections::BTreeSet;

pub(super) fn lower_expression(node: &Expression) -> LowerResult<LoweredExpr> {
    lower_range(&required(node.range(), "expression range", node.syntax())?)
}

fn lower_range(node: &Range) -> LowerResult<LoweredExpr> {
    let operands = node.coalescings().collect::<Vec<_>>();
    let first = required(operands.first().cloned(), "range start", node.syntax())?;
    let start = lower_coalescing(&first)?;
    let operator = node.dot_dot_token().or_else(|| node.dot_dot_eq_token());
    let Some(operator) = operator else {
        return Ok(start);
    };
    let end = lower_coalescing(&required(
        operands.get(1).cloned(),
        "range end",
        node.syntax(),
    )?)?;
    Ok(LoweredExpr {
        kind: LoweredExprKind::Range {
            start: Box::new(start),
            end: Box::new(end),
            inclusive: operator.kind() == SyntaxKind::DotDotEq,
            operator_span: span_token(&operator),
        },
        span: span_node(node.syntax()),
    })
}

fn lower_coalescing(node: &Coalescing) -> LowerResult<LoweredExpr> {
    const VALUE: &str = "__allen_coalesced_value";

    let left = lower_pipeline(&required(
        node.pipeline(),
        "coalescing left operand",
        node.syntax(),
    )?)?;
    let Some(operator) = node.question_question_token() else {
        return Ok(left);
    };
    let right = lower_coalescing(&required(
        node.coalescing(),
        "coalescing right operand",
        node.syntax(),
    )?)?;
    let span = span_node(node.syntax());
    let pattern_span = span_token(&operator);
    Ok(LoweredExpr {
        kind: LoweredExprKind::Match {
            source: Box::new(left),
            arms: vec![
                (
                    LoweredPattern::Option {
                        some: false,
                        payload: None,
                    },
                    right,
                    pattern_span,
                ),
                (
                    LoweredPattern::Option {
                        some: true,
                        payload: Some(Box::new(LoweredPattern::Binding {
                            name: VALUE.to_owned(),
                            span: pattern_span,
                        })),
                    },
                    LoweredExpr {
                        kind: LoweredExprKind::Variable(VALUE.to_owned()),
                        span: pattern_span,
                    },
                    pattern_span,
                ),
            ],
        },
        span,
    })
}

fn lower_pipeline(node: &Pipeline) -> LowerResult<LoweredExpr> {
    fold_call_operator(
        node.compositions(),
        node.pipe_gt_tokens()
            .map(|token| span_token(&token))
            .collect(),
        lower_composition,
        node.syntax(),
        |left, stage, operator_span| LoweredExprKind::Pipe {
            left: Box::new(left),
            stage: Box::new(stage),
            operator_span,
        },
    )
}

fn lower_composition(node: &Composition) -> LowerResult<LoweredExpr> {
    let tokens = node.gt_tokens().collect::<Vec<_>>();
    if tokens.len() % 2 != 0 {
        return Err(malformed_node("composition operator", node.syntax()));
    }
    let operators = tokens
        .chunks_exact(2)
        .map(|pair| Span {
            start: span_token(&pair[0]).start,
            end: span_token(&pair[1]).end,
        })
        .collect();
    fold_call_operator(
        node.disjunctions(),
        operators,
        lower_disjunction,
        node.syntax(),
        |left, right, operator_span| LoweredExprKind::Compose {
            left: Box::new(left),
            right: Box::new(right),
            operator_span,
        },
    )
}

fn fold_call_operator<T>(
    operands: impl IntoIterator<Item = T>,
    mut operators: Vec<Span>,
    lower: impl Fn(&T) -> LowerResult<LoweredExpr>,
    owner: &SyntaxNode,
    make: impl Fn(LoweredExpr, LoweredExpr, Span) -> LoweredExprKind,
) -> LowerResult<LoweredExpr> {
    let operands = operands.into_iter().collect::<Vec<_>>();
    operators.sort_by_key(|span| span.start);
    if operands.len() != operators.len().saturating_add(1) {
        return Err(malformed_node("call operator operand sequence", owner));
    }
    let mut operands = operands.iter();
    let first = operands
        .next()
        .ok_or_else(|| malformed_node("call operator operand", owner))?;
    let mut expression = lower(first)?;
    for (operator_span, operand) in operators.into_iter().zip(operands) {
        let right = lower(operand)?;
        let span = Span {
            start: expression.span.start,
            end: right.span.end,
        };
        expression = LoweredExpr {
            kind: make(expression, right, operator_span),
            span,
        };
    }
    Ok(expression)
}

fn lower_disjunction(node: &Disjunction) -> LowerResult<LoweredExpr> {
    fold_binary(
        node.conjunctions(),
        node.pipe_pipe_tokens().map(|token| (token, Binary::Or)),
        lower_conjunction,
        node.syntax(),
    )
}

fn lower_conjunction(node: &Conjunction) -> LowerResult<LoweredExpr> {
    fold_binary(
        node.equalities(),
        node.amp_amp_tokens().map(|token| (token, Binary::And)),
        lower_equality,
        node.syntax(),
    )
}

fn lower_equality(node: &Equality) -> LowerResult<LoweredExpr> {
    let operators = node
        .eq_eq_tokens()
        .map(|token| (token, Binary::Equal))
        .chain(node.not_eq_tokens().map(|token| (token, Binary::NotEqual)));
    fold_binary(
        node.comparisons(),
        operators,
        lower_comparison,
        node.syntax(),
    )
}

fn lower_comparison(node: &Comparison) -> LowerResult<LoweredExpr> {
    let operators = node
        .lt_tokens()
        .map(|token| (token, Binary::Less))
        .chain(node.lt_eq_tokens().map(|token| (token, Binary::LessEqual)))
        .chain(node.gt_tokens().map(|token| (token, Binary::Greater)))
        .chain(
            node.gt_eq_tokens()
                .map(|token| (token, Binary::GreaterEqual)),
        );
    fold_binary(node.additions(), operators, lower_addition, node.syntax())
}

fn lower_addition(node: &Addition) -> LowerResult<LoweredExpr> {
    let operators = node
        .plus_tokens()
        .map(|token| (token, Binary::Add))
        .chain(node.minus_tokens().map(|token| (token, Binary::Subtract)));
    fold_binary(
        node.multiplications(),
        operators,
        lower_multiplication,
        node.syntax(),
    )
}

fn lower_multiplication(node: &Multiplication) -> LowerResult<LoweredExpr> {
    let operators = node
        .star_tokens()
        .map(|token| (token, Binary::Multiply))
        .chain(node.slash_tokens().map(|token| (token, Binary::Divide)))
        .chain(
            node.percent_tokens()
                .map(|token| (token, Binary::Remainder)),
        );
    fold_binary(node.unaries(), operators, lower_unary, node.syntax())
}

fn fold_binary<T>(
    operands: impl IntoIterator<Item = T>,
    operators: impl IntoIterator<Item = (SyntaxToken, Binary)>,
    lower: impl Fn(&T) -> LowerResult<LoweredExpr>,
    owner: &SyntaxNode,
) -> LowerResult<LoweredExpr> {
    let operands = operands.into_iter().collect::<Vec<_>>();
    let mut operators = operators.into_iter().collect::<Vec<_>>();
    operators.sort_by_key(|(token, _)| token.text_range().start());
    if operands.len() != operators.len().saturating_add(1) {
        return Err(malformed_node("binary operand/operator sequence", owner));
    }
    let mut operands = operands.iter();
    let first = operands
        .next()
        .ok_or_else(|| malformed_node("binary operand", owner))?;
    let mut expression = lower(first)?;
    for ((_, operation), operand) in operators.into_iter().zip(operands) {
        let right = lower(operand)?;
        let span = Span {
            start: expression.span.start,
            end: right.span.end,
        };
        expression = LoweredExpr {
            kind: LoweredExprKind::Binary {
                operation,
                left: Box::new(expression),
                right: Box::new(right),
            },
            span,
        };
    }
    Ok(expression)
}

fn lower_unary(node: &SyntaxUnary) -> LowerResult<LoweredExpr> {
    if let Some(postfix) = node.postfix() {
        return lower_postfix(&postfix);
    }
    let child = required(node.unary(), "unary operand", node.syntax())?;
    if node.minus_token().is_some() && is_minimum_int_magnitude(&child) {
        return Ok(LoweredExpr {
            kind: LoweredExprKind::Int(i64::MIN),
            span: span_node(node.syntax()),
        });
    }
    let operand = lower_unary(&child)?;
    let end = operand.span.end;
    let (start, kind) = if let Some(operator) = node.bang_token() {
        (
            span_token(&operator).start,
            LoweredExprKind::Unary {
                operation: Unary::Not,
                operand: Box::new(operand),
            },
        )
    } else if let Some(operator) = node.minus_token() {
        (
            span_token(&operator).start,
            LoweredExprKind::Unary {
                operation: Unary::Negate,
                operand: Box::new(operand),
            },
        )
    } else if let Some(operator) = node.await_token() {
        (
            span_token(&operator).start,
            LoweredExprKind::Await(Box::new(operand)),
        )
    } else if let Some(operator) = node.spawn_token() {
        (
            span_token(&operator).start,
            LoweredExprKind::Spawn(Box::new(operand)),
        )
    } else {
        return Err(malformed_node("unary operator", node.syntax()));
    };
    Ok(LoweredExpr {
        kind,
        span: Span { start, end },
    })
}

fn is_minimum_int_magnitude(node: &SyntaxUnary) -> bool {
    node.postfix()
        .and_then(|postfix| postfix.primary())
        .and_then(|primary| primary.literal())
        .and_then(|literal| literal.int_literal_token())
        .is_some_and(|token| {
            let digits = token
                .text()
                .bytes()
                .filter(|byte| *byte != b'_')
                .collect::<Vec<_>>();
            let normalized = &digits[digits
                .iter()
                .position(|byte| *byte != b'0')
                .unwrap_or(digits.len())..];
            normalized == b"9223372036854775808"
        })
}

enum PostfixOperation {
    Slice(Slice),
    Field {
        dot: SyntaxToken,
        name: SyntaxToken,
    },
    OptionalField {
        operator: SyntaxToken,
        name: SyntaxToken,
    },
    Call {
        opener: SyntaxToken,
        closer: SyntaxToken,
        type_argument: Option<TypeArgument>,
        arguments: Vec<CallArgument>,
    },
    Try(SyntaxToken),
    TrailingClosure(Closure),
    TrailingShortClosure(ShortClosure),
}

impl PostfixOperation {
    fn start(&self) -> allen_syntax::TextSize {
        match self {
            Self::Slice(slice) => slice.syntax().text_range().start(),
            Self::Call { opener, .. } => opener.text_range().start(),
            Self::Field { dot, .. } => dot.text_range().start(),
            Self::OptionalField { operator, .. } => operator.text_range().start(),
            Self::Try(token) => token.text_range().start(),
            Self::TrailingClosure(closure) => closure.syntax().text_range().start(),
            Self::TrailingShortClosure(closure) => closure.syntax().text_range().start(),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn lower_postfix(node: &Postfix) -> LowerResult<LoweredExpr> {
    let primary = required(node.primary(), "postfix primary", node.syntax())?;
    let mut expression = lower_primary(&primary)?;
    let mut operations = collect_postfix_operations(node)?;
    operations.sort_by_key(PostfixOperation::start);
    for operation in operations {
        let start = expression.span.start;
        match operation {
            PostfixOperation::Slice(slice) => {
                let index = lower_expression(&required(
                    slice.index(),
                    "slice or index expression",
                    slice.syntax(),
                )?)?;
                let bracket_span = span_node(slice.syntax());
                expression = LoweredExpr {
                    kind: if matches!(index.kind, LoweredExprKind::Range { .. }) {
                        LoweredExprKind::Slice {
                            collection: Box::new(expression),
                            range: Box::new(index),
                            bracket_span,
                        }
                    } else {
                        LoweredExprKind::Index {
                            collection: Box::new(expression),
                            index: Box::new(index),
                        }
                    },
                    span: Span {
                        start,
                        end: bracket_span.end,
                    },
                };
            }
            PostfixOperation::Field { name, .. } => {
                let (field, field_span) = checked_field_name(&name, "field name")?;
                expression = LoweredExpr {
                    kind: LoweredExprKind::FieldGet {
                        record: Box::new(expression),
                        field,
                        field_span,
                    },
                    span: Span {
                        start,
                        end: field_span.end,
                    },
                };
            }
            PostfixOperation::OptionalField { operator, name } => {
                let (field, field_span) = checked_field_name(&name, "optional field name")?;
                expression = LoweredExpr {
                    kind: LoweredExprKind::OptionalFieldGet {
                        receiver: Box::new(expression),
                        field,
                        operator_span: span_token(&operator),
                        field_span,
                    },
                    span: Span {
                        start,
                        end: field_span.end,
                    },
                };
            }
            PostfixOperation::Call {
                closer,
                type_argument,
                arguments,
                ..
            } => {
                let type_arguments = type_argument
                    .map(|argument| {
                        super::lower_type(&required(
                            argument.ty(),
                            "call type argument",
                            argument.syntax(),
                        )?)
                    })
                    .into_iter()
                    .collect::<LowerResult<Vec<_>>>()?;
                let arguments = arguments
                    .iter()
                    .map(lower_call_argument)
                    .collect::<LowerResult<_>>()?;
                expression = LoweredExpr {
                    kind: LoweredExprKind::Call {
                        callee: Box::new(expression),
                        type_arguments,
                        arguments,
                    },
                    span: Span {
                        start,
                        end: span_token(&closer).end,
                    },
                };
            }
            PostfixOperation::Try(token) => {
                expression = LoweredExpr {
                    kind: LoweredExprKind::Try(Box::new(expression)),
                    span: Span {
                        start,
                        end: span_token(&token).end,
                    },
                };
            }
            PostfixOperation::TrailingClosure(closure) => {
                expression = append_trailing_callback(expression, lower_closure(&closure)?)?;
            }
            PostfixOperation::TrailingShortClosure(closure) => {
                expression = append_trailing_callback(expression, lower_short_closure(&closure)?)?;
            }
        }
    }
    Ok(expression)
}

fn collect_postfix_operations(node: &Postfix) -> LowerResult<Vec<PostfixOperation>> {
    let mut operations = Vec::new();
    operations.extend(node.slices().map(PostfixOperation::Slice));

    let mut member_operators = node
        .dot_tokens()
        .map(|token| (token, false))
        .chain(node.question_dot_tokens().map(|token| (token, true)))
        .collect::<Vec<_>>();
    member_operators.sort_by_key(|(token, _)| token.text_range().start());
    let names = node
        .syntax()
        .children_with_tokens()
        .filter_map(|element| match element {
            allen_syntax::SyntaxElement::Token(token) => Some(token),
            allen_syntax::SyntaxElement::Node(_) => None,
        })
        .filter(|token| matches!(token.kind(), SyntaxKind::Ident | SyntaxKind::KwMap))
        .collect::<Vec<_>>();
    if member_operators.len() != names.len() {
        return Err(malformed_node("postfix field", node.syntax()));
    }
    operations.extend(member_operators.into_iter().zip(names).map(
        |((operator, optional), name)| {
            if optional {
                PostfixOperation::OptionalField { operator, name }
            } else {
                PostfixOperation::Field {
                    dot: operator,
                    name,
                }
            }
        },
    ));

    let calls = pair_tokens(
        node.l_paren_tokens().collect(),
        node.r_paren_tokens().collect(),
        "postfix call delimiters",
        node.syntax(),
    )?;
    let arguments = node.call_arguments().collect::<Vec<_>>();
    let type_arguments = node.call_type_arguments().collect::<Vec<_>>();
    let mut previous_call_end = allen_syntax::TextSize::from(0);
    for (opener, closer) in calls {
        let open = opener.text_range().start();
        let close = closer.text_range().start();
        let argument_group = arguments
            .iter()
            .filter(|argument| {
                let range = argument.syntax().text_range();
                range.start() > open && range.end() <= close
            })
            .cloned()
            .collect();
        let candidates = type_arguments
            .iter()
            .filter(|argument| {
                let range = argument.syntax().text_range();
                range.start() >= previous_call_end && range.end() <= open
            })
            .cloned()
            .collect::<Vec<_>>();
        if candidates.len() > 1 {
            return Err(malformed_node("call type argument", node.syntax()));
        }
        operations.push(PostfixOperation::Call {
            opener,
            closer: closer.clone(),
            type_argument: candidates.into_iter().next(),
            arguments: argument_group,
        });
        previous_call_end = closer.text_range().end();
    }
    operations.extend(node.question_tokens().map(PostfixOperation::Try));
    operations.extend(node.closures().map(PostfixOperation::TrailingClosure));
    operations.extend(
        node.short_closures()
            .map(PostfixOperation::TrailingShortClosure),
    );
    Ok(operations)
}

fn append_trailing_callback(
    mut call: LoweredExpr,
    callback: LoweredExpr,
) -> LowerResult<LoweredExpr> {
    let call_span = call.span;
    let callback_span = callback.span;
    let LoweredExprKind::Call { arguments, .. } = &mut call.kind else {
        return Err(SyntaxLoweringError::MalformedTree {
            expected: "trailing callback call",
            span: callback_span,
        });
    };
    arguments.push(LoweredCallArgument {
        label: None,
        value: callback,
        placeholder: false,
        trailing: true,
        preceding_call_span: Some(call_span),
        span: callback_span,
    });
    call.span.end = callback_span.end;
    Ok(call)
}

fn checked_field_name(token: &SyntaxToken, expected: &'static str) -> LowerResult<(String, Span)> {
    if token.kind() == SyntaxKind::KwMap {
        return Ok((token.text().to_owned(), span_token(token)));
    }
    if token.kind() != SyntaxKind::Ident {
        return checked_ident(token, expected);
    }
    let name = token.text().to_owned();
    if is_forbidden_source_word(&name, SourceWordContext::MemberName) {
        return Err(compiler_error(
            "E2020",
            format!("'{name}' is forbidden; use Option<T> or unknown"),
            span_token(token),
        ));
    }
    Ok((name, span_token(token)))
}

fn pair_tokens(
    mut openers: Vec<SyntaxToken>,
    mut closers: Vec<SyntaxToken>,
    expected: &'static str,
    owner: &SyntaxNode,
) -> LowerResult<Vec<(SyntaxToken, SyntaxToken)>> {
    openers.sort_by_key(|token| token.text_range().start());
    closers.sort_by_key(|token| token.text_range().start());
    if openers.len() != closers.len() {
        return Err(malformed_node(expected, owner));
    }
    Ok(openers.into_iter().zip(closers).collect())
}

fn lower_call_argument(node: &CallArgument) -> LowerResult<LoweredCallArgument> {
    let label = node
        .argument_label_token()
        .map(|token| checked_ident(&token, "call argument label"))
        .transpose()?;
    let placeholder = node.underscore_token().is_some();
    let value = if placeholder {
        LoweredExpr {
            kind: LoweredExprKind::Variable("_".to_owned()),
            span: span_node(node.syntax()),
        }
    } else {
        lower_expression(&required(
            node.value(),
            "call argument expression",
            node.syntax(),
        )?)?
    };
    Ok(LoweredCallArgument {
        label,
        span: span_node(node.syntax()),
        value,
        placeholder,
        trailing: false,
        preceding_call_span: None,
    })
}

fn lower_primary(node: &Primary) -> LowerResult<LoweredExpr> {
    if let Some(literal) = node.literal() {
        lower_literal(&literal)
    } else if let Some(template) = node.template_literal() {
        lower_template(&template)
    } else if let Some(token) = node.ident_token() {
        let (name, span) = checked_ident(&token, "variable")?;
        Ok(LoweredExpr {
            kind: LoweredExprKind::Variable(name),
            span,
        })
    } else if let Some(token) = node.map_token() {
        Ok(LoweredExpr {
            kind: LoweredExprKind::Variable(token.text().to_owned()),
            span: span_token(&token),
        })
    } else if let Some(token) = node
        .some_token()
        .or_else(|| node.ok_token())
        .or_else(|| node.err_token())
    {
        Ok(LoweredExpr {
            kind: LoweredExprKind::Variable(token.text().to_owned()),
            span: span_token(&token),
        })
    } else if let Some(constructor) = node.enum_record_constructor() {
        lower_enum_record_constructor(&constructor)
    } else if let Some(qualified) = node.qualified_enum() {
        lower_qualified_enum(&qualified)
    } else if let Some(constructor) = node.record_constructor() {
        lower_record_constructor(&constructor)
    } else if let Some(record) = node.anonymous_record() {
        lower_anonymous_record(&record)
    } else if let Some(list) = node.list_literal() {
        lower_list(&list)
    } else if let Some(map) = node.map_literal() {
        lower_map(&map)
    } else if let Some(tuple) = node.tuple_or_group() {
        lower_tuple_or_group(&tuple)
    } else if let Some(expression) = node.match_expression() {
        lower_match(&expression)
    } else if let Some(conditional) = node.conditional_expression() {
        lower_conditional(&conditional)
    } else if let Some(closure) = node.closure() {
        lower_closure(&closure)
    } else if let Some(closure) = node.short_closure() {
        lower_short_closure(&closure)
    } else if let Some(prompt) = node.prompt_expression() {
        lower_prompt(&prompt)
    } else if let Some(await_block) = node.await_block() {
        lower_await_block(&await_block)
    } else {
        Err(malformed_node("primary expression", node.syntax()))
    }
}

fn lower_short_closure(node: &ShortClosure) -> LowerResult<LoweredExpr> {
    let parameters = node
        .parameter_names_tokens()
        .map(|token| checked_ident(&token, "concise lambda parameter"))
        .collect::<LowerResult<_>>()?;
    let body = lower_expression(&required(
        node.body(),
        "concise lambda body",
        node.syntax(),
    )?)?;
    Ok(LoweredExpr {
        kind: LoweredExprKind::ShortClosure {
            parameters,
            body: Box::new(body),
        },
        span: span_node(node.syntax()),
    })
}

fn lower_literal(node: &Literal) -> LowerResult<LoweredExpr> {
    if let Some(token) = node.int_literal_token() {
        let span = span_token(&token);
        let value = token.text().replace('_', "").parse::<i64>().map_err(|_| {
            compiler_error(
                "E3005",
                "integer literal is out of range unless immediately preceded by '-'",
                span,
            )
        })?;
        Ok(LoweredExpr {
            kind: LoweredExprKind::Int(value),
            span,
        })
    } else if let Some(token) = node.float_literal_token() {
        let span = span_token(&token);
        let value = token
            .text()
            .replace('_', "")
            .parse::<f64>()
            .map_err(|_| compiler_error("E0003", "invalid Float literal", span))?;
        if !value.is_finite() {
            return Err(compiler_error("E0003", "invalid Float literal", span));
        }
        Ok(LoweredExpr {
            kind: LoweredExprKind::Float(canonical_float_bits(value.to_bits())),
            span,
        })
    } else if let Some(token) = node.string_literal_token() {
        let span = span_token(&token);
        Ok(LoweredExpr {
            kind: LoweredExprKind::String(decode_string(token.text(), span)?),
            span,
        })
    } else if let Some(token) = node.bytes_literal_token() {
        let span = span_token(&token);
        Ok(LoweredExpr {
            kind: LoweredExprKind::Bytes(decode_bytes(token.text(), span)?),
            span,
        })
    } else if let Some(token) = node.true_token().or_else(|| node.false_token()) {
        Ok(LoweredExpr {
            kind: LoweredExprKind::Bool(token.kind() == SyntaxKind::KwTrue),
            span: span_token(&token),
        })
    } else if let Some(token) = node.none_token() {
        Ok(LoweredExpr {
            kind: LoweredExprKind::Variable("None".to_owned()),
            span: span_token(&token),
        })
    } else if node.l_paren_token().is_some() && node.r_paren_token().is_some() {
        Ok(LoweredExpr {
            kind: LoweredExprKind::Unit,
            span: span_node(node.syntax()),
        })
    } else {
        Err(malformed_node("literal alternative", node.syntax()))
    }
}

fn lower_template(node: &TemplateLiteral) -> LowerResult<LoweredExpr> {
    let opener = node
        .open_backtick_token()
        .or_else(|| node.open_multiline_delimiter_token())
        .ok_or_else(|| malformed_node("template opener", node.syntax()))?;
    let closer = node
        .close_backtick_token()
        .or_else(|| node.close_multiline_delimiter_token())
        .ok_or_else(|| malformed_node("template closer", node.syntax()))?;
    let multiline = opener.kind() == SyntaxKind::MultilineStringDelimiter;
    let mut segments = node.template_segments().collect::<Vec<_>>();
    segments.sort_by_key(|segment| segment.syntax().text_range().start());
    let mut interpolations = node.template_interpolations().collect::<Vec<_>>();
    interpolations.sort_by_key(|part| part.syntax().text_range().start());
    let mut parts = Vec::new();
    let mut cursor = opener.text_range().end();
    let mut segments = segments.into_iter().peekable();
    for interpolation in interpolations {
        let interpolation_start = interpolation.syntax().text_range().start();
        let mut value = String::new();
        while segments
            .peek()
            .is_some_and(|segment| segment.syntax().text_range().end() <= interpolation_start)
        {
            value.push_str(&decode_template_segment(
                &segments.next().expect("peeked template segment"),
            )?);
        }
        parts.push(LoweredTemplatePart::Literal {
            value,
            span: Span {
                start: u32::from(cursor) as usize,
                end: u32::from(interpolation_start) as usize,
            },
        });
        parts.push(LoweredTemplatePart::Interpolation(
            lower_template_interpolation(&interpolation)?,
        ));
        cursor = interpolation.syntax().text_range().end();
    }
    let mut value = String::new();
    for segment in segments {
        value.push_str(&decode_template_segment(&segment)?);
    }
    parts.push(LoweredTemplatePart::Literal {
        value,
        span: Span {
            start: u32::from(cursor) as usize,
            end: u32::from(closer.text_range().start()) as usize,
        },
    });
    if multiline {
        trim_multiline_template(&mut parts, span_node(node.syntax()))?;
    }
    Ok(LoweredExpr {
        kind: LoweredExprKind::Template(parts),
        span: Span {
            start: span_token(&opener).start,
            end: span_token(&closer).end,
        },
    })
}

fn trim_multiline_template(parts: &mut [LoweredTemplatePart], span: Span) -> LowerResult<()> {
    let literal_indexes = parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| {
            matches!(part, LoweredTemplatePart::Literal { .. }).then_some(index)
        })
        .collect::<Vec<_>>();
    for index in &literal_indexes {
        if let LoweredTemplatePart::Literal { value, .. } = &mut parts[*index] {
            *value = value.replace("\r\n", "\n").replace('\r', "\n");
        }
    }
    if let Some(first) = literal_indexes.first() {
        if let LoweredTemplatePart::Literal { value, .. } = &mut parts[*first] {
            *value = value.strip_prefix('\n').unwrap_or(value).to_owned();
        }
    }
    let mut closing_indentation = String::new();
    if let Some(last) = literal_indexes.last() {
        if let LoweredTemplatePart::Literal { value, .. } = &mut parts[*last] {
            value
                .rsplit_once('\n')
                .map_or_else(|| value.as_str(), |(_, indentation)| indentation)
                .clone_into(&mut closing_indentation);
            if !closing_indentation
                .bytes()
                .all(|byte| matches!(byte, b' ' | b'\t'))
            {
                return Err(compiler_error(
                    "E3005",
                    "multiline string closing delimiter must begin on its own line",
                    span,
                ));
            }
            let without_closing_indent = value.trim_end_matches([' ', '\t']);
            *value = without_closing_indent
                .strip_suffix('\n')
                .unwrap_or(without_closing_indent)
                .to_owned();
        }
    }

    let indentation = parts
        .iter()
        .filter_map(|part| match part {
            LoweredTemplatePart::Literal { value, .. } => Some(value),
            LoweredTemplatePart::Interpolation(_) => None,
        })
        .flat_map(|value| value.split('\n'))
        .filter(|line| !line.trim_matches([' ', '\t']).is_empty())
        .map(|line| {
            line.bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count()
        })
        .min();
    let Some(indentation) = indentation else {
        return Ok(());
    };
    let closing_width = closing_indentation.len();
    if indentation < closing_width {
        return Err(compiler_error(
            "E3005",
            "multiline string content is less indented than its closing delimiter",
            span,
        ));
    }
    let indentation = indentation.min(closing_width);
    if indentation == 0 {
        return Ok(());
    }
    for index in literal_indexes {
        let LoweredTemplatePart::Literal { value, .. } = &mut parts[index] else {
            continue;
        };
        *value = value
            .split('\n')
            .map(|line| {
                let prefix = line
                    .bytes()
                    .take_while(|byte| matches!(byte, b' ' | b'\t'))
                    .count();
                if !line.trim_matches([' ', '\t']).is_empty() && prefix >= indentation {
                    &line[indentation..]
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    Ok(())
}

fn decode_template_segment(node: &TemplateSegment) -> LowerResult<String> {
    let mut value = String::new();
    for piece in node.template_text_or_escapes() {
        if let Some(token) = piece.template_text_scalar_token() {
            value.push_str(token.text());
        } else if let Some(token) = piece.template_escape_token() {
            value.push_str(&decode_template_escape(token.text(), span_token(&token))?);
        } else {
            return Err(malformed_node("template text or escape", piece.syntax()));
        }
    }
    Ok(value)
}

fn decode_template_escape(text: &str, span: Span) -> LowerResult<String> {
    if text == r"\${" {
        return Ok("${".to_owned());
    }
    if text == r"\`" {
        return Ok("`".to_owned());
    }
    let bytes = decode_escape(text.as_bytes().get(1).copied(), false, &[], span)?;
    Ok(char::from(bytes.0).to_string())
}

fn lower_template_interpolation(node: &TemplateInterpolation) -> LowerResult<LoweredExpr> {
    lower_expression(&required(
        node.expression(),
        "template interpolation expression",
        node.syntax(),
    )?)
}

fn lower_enum_record_constructor(node: &EnumRecordConstructor) -> LowerResult<LoweredExpr> {
    let (name, _) = required_ident(node.enum_name_token(), "enum name", node.syntax())?;
    let (variant, _) = required_ident(node.variant_name_token(), "variant name", node.syntax())?;
    Ok(LoweredExpr {
        kind: LoweredExprKind::Enum {
            name,
            variant,
            payload: LoweredEnumValuePayload::Record(lower_record_value_fields(
                node.record_value_fields(),
            )?),
        },
        span: span_node(node.syntax()),
    })
}

fn lower_qualified_enum(node: &QualifiedEnum) -> LowerResult<LoweredExpr> {
    let (name, name_span) = required_ident(node.enum_name_token(), "enum name", node.syntax())?;
    let (field, field_span) =
        required_member_name(node.variant_name_token(), "variant name", node.syntax())?;
    let record = LoweredExpr {
        kind: LoweredExprKind::Variable(name),
        span: name_span,
    };
    Ok(LoweredExpr {
        kind: LoweredExprKind::FieldGet {
            record: Box::new(record),
            field,
            field_span,
        },
        span: span_node(node.syntax()),
    })
}

fn required_member_name(
    token: Option<SyntaxToken>,
    expected: &'static str,
    owner: &SyntaxNode,
) -> LowerResult<(String, Span)> {
    let token = token.ok_or_else(|| malformed_node(expected, owner))?;
    checked_field_name(&token, expected)
}

fn lower_record_constructor(node: &RecordConstructor) -> LowerResult<LoweredExpr> {
    let (name, _) = required_ident(node.ident_token(), "record name", node.syntax())?;
    let fields = lower_record_value_fields(node.record_value_fields())?;
    if let Some(update) = node.record_update_base() {
        let spread = required(
            update.dot_dot_token(),
            "record update spread",
            update.syntax(),
        )?;
        let base = lower_expression(&required(
            update.base(),
            "record update base",
            update.syntax(),
        )?)?;
        return Ok(LoweredExpr {
            kind: LoweredExprKind::RecordUpdate {
                name,
                base: Box::new(base),
                spread_span: span_token(&spread),
                fields,
            },
            span: span_node(node.syntax()),
        });
    }
    Ok(LoweredExpr {
        kind: LoweredExprKind::Record { name, fields },
        span: span_node(node.syntax()),
    })
}

fn lower_anonymous_record(node: &AnonymousRecord) -> LowerResult<LoweredExpr> {
    let fields = lower_record_value_fields(node.record_value_fields())?;
    let names = fields
        .iter()
        .map(|(name, _, _)| name.as_str())
        .collect::<BTreeSet<_>>();
    let name = if names == BTreeSet::from(["access", "path", "reason"]) {
        "ExternalFileRequest"
    } else if names == BTreeSet::from(["access", "path", "reason", "recursive"]) {
        "ExternalDirectoryRequest"
    } else {
        "$anonymous"
    };
    let kind = if let Some(update) = node.record_update_base() {
        let spread = required(
            update.dot_dot_token(),
            "record update spread",
            update.syntax(),
        )?;
        let base = lower_expression(&required(
            update.base(),
            "record update base",
            update.syntax(),
        )?)?;
        LoweredExprKind::RecordUpdate {
            name: name.to_owned(),
            base: Box::new(base),
            spread_span: span_token(&spread),
            fields,
        }
    } else {
        LoweredExprKind::Record {
            name: name.to_owned(),
            fields,
        }
    };
    Ok(LoweredExpr {
        kind,
        span: span_node(node.syntax()),
    })
}

fn lower_record_value_fields(
    fields: impl IntoIterator<Item = RecordValueField>,
) -> LowerResult<Vec<(String, LoweredExpr, Span)>> {
    fields
        .into_iter()
        .map(|field| {
            let (name, span) =
                required_ident(field.ident_token(), "record value field", field.syntax())?;
            let value = field.expression().map_or_else(
                || {
                    Ok(LoweredExpr {
                        kind: LoweredExprKind::Variable(name.clone()),
                        span,
                    })
                },
                |expression| lower_expression(&expression),
            )?;
            Ok((name, value, span))
        })
        .collect()
}

fn lower_list(node: &ListLiteral) -> LowerResult<LoweredExpr> {
    let items = node
        .list_items()
        .map(|item| lower_list_item(&item))
        .collect::<LowerResult<Vec<_>>>()?;
    if items.iter().any(|item| item.spread) {
        return Ok(LoweredExpr {
            kind: LoweredExprKind::ListWithSpread(items),
            span: span_node(node.syntax()),
        });
    }
    Ok(LoweredExpr {
        kind: LoweredExprKind::List(items.into_iter().map(|item| item.value).collect()),
        span: span_node(node.syntax()),
    })
}

fn lower_list_item(node: &ListItem) -> LowerResult<LoweredListItem> {
    let spread = node.dot_dot_token().is_some();
    let expression = if spread { node.spread() } else { node.value() };
    Ok(LoweredListItem {
        spread,
        value: lower_expression(&required(expression, "list item", node.syntax())?)?,
        span: span_node(node.syntax()),
    })
}

fn lower_map(node: &MapLiteral) -> LowerResult<LoweredExpr> {
    let items = node
        .map_items()
        .map(|item| lower_map_item(&item))
        .collect::<LowerResult<Vec<_>>>()?;
    if items
        .iter()
        .any(|item| matches!(item, LoweredMapItem::Spread { .. }))
    {
        return Ok(LoweredExpr {
            kind: LoweredExprKind::MapWithSpread(items),
            span: span_node(node.syntax()),
        });
    }
    let entries = items
        .into_iter()
        .map(|item| match item {
            LoweredMapItem::Entry { key, value, .. } => Ok((key, value)),
            LoweredMapItem::Spread { .. } => {
                Err(malformed_node("ordinary map entry", node.syntax()))
            }
        })
        .collect::<LowerResult<_>>()?;
    Ok(LoweredExpr {
        kind: LoweredExprKind::Map(entries),
        span: span_node(node.syntax()),
    })
}

fn lower_map_item(node: &MapItem) -> LowerResult<LoweredMapItem> {
    let span = span_node(node.syntax());
    if node.dot_dot_token().is_some() {
        return Ok(LoweredMapItem::Spread {
            value: lower_expression(&required(node.spread(), "map spread", node.syntax())?)?,
            span,
        });
    }
    Ok(LoweredMapItem::Entry {
        key: lower_expression(&required(node.key(), "map entry key", node.syntax())?)?,
        value: lower_expression(&required(node.value(), "map entry value", node.syntax())?)?,
        span,
    })
}

fn lower_tuple_or_group(node: &TupleOrGroup) -> LowerResult<LoweredExpr> {
    let expressions = node.expressions().collect::<Vec<_>>();
    if node.comma_tokens().next().is_none() {
        let expression = expressions
            .first()
            .ok_or_else(|| malformed_node("grouped expression", node.syntax()))?;
        return lower_expression(expression);
    }
    Ok(LoweredExpr {
        kind: LoweredExprKind::Tuple(
            expressions
                .iter()
                .map(lower_expression)
                .collect::<LowerResult<_>>()?,
        ),
        span: span_node(node.syntax()),
    })
}

fn lower_match(node: &MatchExpression) -> LowerResult<LoweredExpr> {
    let source = lower_expression(&required(
        node.scrutinee(),
        "match scrutinee",
        node.syntax(),
    )?)?;
    let arms = node
        .match_arms()
        .map(|arm| {
            let pattern_node = required(arm.pattern(), "match pattern", arm.syntax())?;
            let (pattern, span) = lower_pattern(&pattern_node)?;
            let value = lower_expression(&required(
                arm.expression(),
                "match arm value",
                arm.syntax(),
            )?)?;
            Ok((pattern, value, span))
        })
        .collect::<LowerResult<_>>()?;
    Ok(LoweredExpr {
        kind: LoweredExprKind::Match {
            source: Box::new(source),
            arms,
        },
        span: span_node(node.syntax()),
    })
}

fn lower_pattern(node: &Pattern) -> LowerResult<(LoweredPattern, Span)> {
    let span = span_node(node.syntax());
    let pattern = lower_pattern_or(&required(node.pattern_or(), "OR pattern", node.syntax())?)?;
    Ok((pattern, span))
}

fn lower_pattern_or(node: &PatternOr) -> LowerResult<LoweredPattern> {
    let alternatives = node
        .pattern_primaries()
        .map(|pattern| lower_pattern_primary(&pattern))
        .collect::<LowerResult<Vec<_>>>()?;
    if alternatives.len() == 1 {
        return Ok(alternatives.into_iter().next().expect("one alternative"));
    }
    if alternatives.is_empty() {
        return Err(malformed_node("OR pattern alternative", node.syntax()));
    }
    Ok(LoweredPattern::Or {
        alternatives,
        operator_spans: node.pipe_tokens().map(|token| span_token(&token)).collect(),
    })
}

fn lower_pattern_primary(node: &PatternPrimary) -> LowerResult<LoweredPattern> {
    let pattern = if node.some_token().is_some() {
        LoweredPattern::Option {
            some: true,
            payload: Some(Box::new(
                lower_pattern(&required(
                    node.pattern(),
                    "option payload pattern",
                    node.syntax(),
                )?)?
                .0,
            )),
        }
    } else if node.ok_token().is_some() || node.err_token().is_some() {
        LoweredPattern::Result {
            ok: node.ok_token().is_some(),
            payload: Box::new(
                lower_pattern(&required(
                    node.pattern(),
                    "result payload pattern",
                    node.syntax(),
                )?)?
                .0,
            ),
        }
    } else if node.none_token().is_some() {
        LoweredPattern::Option {
            some: false,
            payload: None,
        }
    } else if node.underscore_token().is_some() {
        LoweredPattern::Wildcard
    } else if node.true_token().is_some() || node.false_token().is_some() {
        LoweredPattern::Bool(node.true_token().is_some())
    } else if let Some(token) = node.binding_name_token() {
        let (name, span) = checked_ident(&token, "pattern binding")?;
        LoweredPattern::Binding { name, span }
    } else if let Some(record) = node.record_pattern() {
        lower_record_pattern(&record)?
    } else if let Some(enumeration) = node.enum_pattern() {
        lower_enum_pattern(&enumeration)?
    } else if let Some(range) = node.pattern_range() {
        lower_pattern_range(&range)?
    } else {
        return Err(malformed_node("primary pattern alternative", node.syntax()));
    };
    Ok(pattern)
}

fn lower_pattern_range(node: &PatternRange) -> LowerResult<LoweredPattern> {
    let literals = node.literals().collect::<Vec<_>>();
    let operator = required(
        node.dot_dot_token().or_else(|| node.dot_dot_eq_token()),
        "range-pattern operator",
        node.syntax(),
    )?;
    let operator_span = span_token(&operator);
    let minus_spans = node
        .minus_tokens()
        .map(|token| span_token(&token))
        .collect::<Vec<_>>();
    let start = lower_pattern_range_endpoint(
        &required(
            literals.first().cloned(),
            "range-pattern start literal",
            node.syntax(),
        )?,
        minus_spans
            .iter()
            .find(|span| span.start < operator_span.start)
            .copied(),
    )?;
    let end = lower_pattern_range_endpoint(
        &required(
            literals.get(1).cloned(),
            "range-pattern end literal",
            node.syntax(),
        )?,
        minus_spans
            .iter()
            .find(|span| span.start >= operator_span.end)
            .copied(),
    )?;
    Ok(LoweredPattern::Range {
        start,
        end,
        inclusive: operator.kind() == SyntaxKind::DotDotEq,
        operator_span,
    })
}

fn lower_pattern_range_endpoint(
    literal: &Literal,
    minus_span: Option<Span>,
) -> LowerResult<LoweredExpr> {
    let Some(minus_span) = minus_span else {
        return lower_literal(literal);
    };
    let token = required(
        literal.int_literal_token(),
        "Int range-pattern endpoint after '-'",
        literal.syntax(),
    )?;
    let literal_span = span_token(&token);
    let magnitude =
        token.text().replace('_', "").parse::<u64>().map_err(|_| {
            compiler_error("E3005", "integer literal is out of range", literal_span)
        })?;
    let value = if magnitude == (i64::MAX as u64) + 1 {
        i64::MIN
    } else {
        i64::try_from(magnitude)
            .ok()
            .and_then(i64::checked_neg)
            .ok_or_else(|| {
                compiler_error("E3005", "integer literal is out of range", literal_span)
            })?
    };
    Ok(LoweredExpr {
        kind: LoweredExprKind::Int(value),
        span: Span {
            start: minus_span.start,
            end: literal_span.end,
        },
    })
}

fn lower_record_pattern(node: &RecordPattern) -> LowerResult<LoweredPattern> {
    let (name, _) = required_ident(node.ident_token(), "record pattern name", node.syntax())?;
    Ok(LoweredPattern::Record {
        name,
        fields: lower_pattern_fields(node.pattern_fields())?,
    })
}

fn lower_enum_pattern(node: &EnumPattern) -> LowerResult<LoweredPattern> {
    let (name, _) = required_ident(node.enum_name_token(), "enum pattern name", node.syntax())?;
    let (variant, _) = required_ident(
        node.variant_name_token(),
        "enum pattern variant",
        node.syntax(),
    )?;
    let patterns = node
        .patterns()
        .map(|pattern| lower_pattern(&pattern).map(|value| value.0))
        .collect::<LowerResult<_>>()?;
    let fields = node
        .l_brace_token()
        .is_some()
        .then(|| lower_pattern_fields(node.pattern_fields()))
        .transpose()?;
    Ok(LoweredPattern::Enum {
        name,
        variant,
        patterns,
        fields,
    })
}

fn lower_pattern_fields(
    fields: impl IntoIterator<Item = PatternField>,
) -> LowerResult<Vec<(String, Span, Box<LoweredPattern>)>> {
    fields
        .into_iter()
        .map(|field| {
            let (name, span) =
                required_ident(field.field_name_token(), "pattern field", field.syntax())?;
            let pattern = if field.colon_token().is_some() {
                Box::new(
                    lower_pattern(&required(
                        field.pattern(),
                        "record field pattern",
                        field.syntax(),
                    )?)?
                    .0,
                )
            } else {
                Box::new(LoweredPattern::Binding {
                    name: name.clone(),
                    span,
                })
            };
            Ok((name, span, pattern))
        })
        .collect()
}

pub(super) fn lower_conditional(node: &ConditionalExpression) -> LowerResult<LoweredExpr> {
    let condition = lower_expression(&required(node.condition(), "if condition", node.syntax())?)?;
    let then_body = super::lower_body(&required(node.then_branch(), "if body", node.syntax())?)?;
    let else_branch = if let Some(else_if) = node.else_if() {
        Some(LoweredElse::If(Box::new(lower_conditional(&else_if)?)))
    } else {
        node.else_branch()
            .map(|body| super::lower_body(&body).map(|body| LoweredElse::Body(Box::new(body))))
            .transpose()?
    };
    Ok(LoweredExpr {
        kind: LoweredExprKind::If {
            condition: Box::new(condition),
            then_body: Box::new(then_body),
            else_branch,
        },
        span: span_node(node.syntax()),
    })
}

fn lower_closure(node: &allen_syntax::Closure) -> LowerResult<LoweredExpr> {
    let parameters = node
        .parameters()
        .map(|parameter| super::lower_parameter(&parameter))
        .collect::<LowerResult<_>>()?;
    let return_type =
        super::lower_type(&required(node.ty(), "closure return type", node.syntax())?)?;
    let (effects, _) = super::lower_optional_effects(node.effect_clause())?;
    let body = super::lower_body(&required(node.body(), "closure body", node.syntax())?)?;
    Ok(LoweredExpr {
        kind: LoweredExprKind::Closure {
            parameters,
            return_type,
            declared_effects: Some(effects.unwrap_or_default()),
            body: Box::new(body),
        },
        span: span_node(node.syntax()),
    })
}

fn lower_prompt(node: &PromptExpression) -> LowerResult<LoweredExpr> {
    let mut system = None;
    let mut context = None;
    let mut data = None;
    let mut output = None;
    let mut max_attempts = 3_u32;
    let mut seen = BTreeSet::new();
    for field in node.prompt_fields() {
        let (name, field_span) = if let Some(token) = field.system_token() {
            ("system", span_token(&token))
        } else if let Some(token) = field.context_token() {
            ("context", span_token(&token))
        } else if let Some(token) = field.data_token() {
            ("data", span_token(&token))
        } else if let Some(token) = field.output_token() {
            ("output", span_token(&token))
        } else if let Some(token) = field.policy_token() {
            ("policy", span_token(&token))
        } else {
            return Err(malformed_node("prompt field alternative", field.syntax()));
        };
        if !seen.insert(name) {
            return Err(compiler_error(
                "E3005",
                format!("prompt repeats '{name}'"),
                field_span,
            ));
        }
        match name {
            "system" => {
                system = Some(Box::new(lower_expression(&required(
                    field.system_value(),
                    "prompt system",
                    field.syntax(),
                )?)?));
            }
            "context" => {
                context = Some(Box::new(lower_expression(&required(
                    field.context_value(),
                    "prompt context",
                    field.syntax(),
                )?)?));
            }
            "data" => {
                data = Some(Box::new(lower_expression(&required(
                    field.data_value(),
                    "prompt data",
                    field.syntax(),
                )?)?));
            }
            "output" => {
                output = Some(super::lower_type(&required(
                    field.output_type(),
                    "prompt output type",
                    field.syntax(),
                )?)?);
            }
            "policy" => {
                let token = required(
                    field.max_attempts_value_token(),
                    "prompt max_attempts",
                    field.syntax(),
                )?;
                max_attempts = token
                    .text()
                    .parse::<u32>()
                    .ok()
                    .filter(|value| (1..=3).contains(value))
                    .ok_or_else(|| {
                        compiler_error(
                            "E3011",
                            "prompt max_attempts must be from 1 through 3",
                            span_token(&token),
                        )
                    })?;
            }
            _ => unreachable!("prompt field names are exhaustive"),
        }
    }
    let span = span_node(node.syntax());
    Ok(LoweredExpr {
        kind: LoweredExprKind::Prompt {
            system: system
                .ok_or_else(|| compiler_error("E3005", "prompt requires system", span))?,
            context,
            data,
            output: output
                .ok_or_else(|| compiler_error("E3005", "prompt requires output", span))?,
            max_attempts,
        },
        span,
    })
}

fn lower_await_block(node: &AwaitBlock) -> LowerResult<LoweredExpr> {
    Ok(LoweredExpr {
        kind: LoweredExprKind::AwaitBlock(Box::new(super::lower_body(&required(
            node.body(),
            "await block body",
            node.syntax(),
        )?)?)),
        span: span_node(node.syntax()),
    })
}

fn decode_escape(
    escape: Option<u8>,
    allow_hex: bool,
    hex: &[u8],
    span: Span,
) -> LowerResult<(u8, usize)> {
    let value = match escape {
        Some(b'"') => b'"',
        Some(b'\\') => b'\\',
        Some(b'n') => b'\n',
        Some(b'r') => b'\r',
        Some(b't') => b'\t',
        Some(b'0') => b'\0',
        Some(b'b') => 0x08,
        Some(b'f') => 0x0c,
        Some(b'x') if allow_hex && hex.len() >= 2 => {
            let high = hex_digit(hex[0]);
            let low = hex_digit(hex[1]);
            match (high, low) {
                (Some(high), Some(low)) => high * 16 + low,
                _ => return Err(compiler_error("E0004", "invalid byte escape", span)),
            }
        }
        _ => return Err(compiler_error("E0004", "unsupported escape", span)),
    };
    Ok((value, usize::from(escape == Some(b'x')) * 2))
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
