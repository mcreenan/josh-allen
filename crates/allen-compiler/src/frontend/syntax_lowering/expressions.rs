use super::{
    Binary, LowerResult, LoweredElse, LoweredEnumValuePayload, LoweredExpr, LoweredExprKind,
    LoweredPattern, LoweredTemplatePart, Span, Unary, checked_ident, compiler_error, decode_bytes,
    decode_string, malformed_node, required, required_ident, span_node, span_token,
};
use allen_bytecode::canonical_float_bits;
use allen_syntax::{
    Addition, AnonymousRecord, AstNode, AwaitBlock, Comparison, ConditionalExpression, Conjunction,
    Disjunction, EnumPattern, EnumRecordConstructor, Equality, Expression, ListLiteral, Literal,
    MapLiteral, MatchExpression, Multiplication, Pattern, PatternField, Postfix, Primary,
    PromptExpression, QualifiedEnum, RecordConstructor, RecordPattern, RecordValueField,
    SyntaxKind, SyntaxNode, SyntaxToken, TemplateInterpolation, TemplateLiteral, TemplateSegment,
    TupleOrGroup, TypeArgument, Unary as SyntaxUnary,
};
use std::collections::BTreeSet;

pub(super) fn lower_expression(node: &Expression) -> LowerResult<LoweredExpr> {
    lower_disjunction(&required(
        node.disjunction(),
        "expression disjunction",
        node.syntax(),
    )?)
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
        .is_some_and(|token| token.text() == "9223372036854775808")
}

enum PostfixOperation {
    Index {
        opener: SyntaxToken,
        closer: SyntaxToken,
        index: Expression,
    },
    Field {
        dot: SyntaxToken,
        name: SyntaxToken,
    },
    Call {
        opener: SyntaxToken,
        closer: SyntaxToken,
        type_argument: Option<TypeArgument>,
        arguments: Vec<Expression>,
    },
    Try(SyntaxToken),
}

impl PostfixOperation {
    fn start(&self) -> allen_syntax::TextSize {
        match self {
            Self::Index { opener, .. } | Self::Call { opener, .. } => opener.text_range().start(),
            Self::Field { dot, .. } => dot.text_range().start(),
            Self::Try(token) => token.text_range().start(),
        }
    }
}

fn lower_postfix(node: &Postfix) -> LowerResult<LoweredExpr> {
    let primary = required(node.primary(), "postfix primary", node.syntax())?;
    let mut expression = lower_primary(&primary)?;
    let mut operations = collect_postfix_operations(node)?;
    operations.sort_by_key(PostfixOperation::start);
    for operation in operations {
        let start = expression.span.start;
        match operation {
            PostfixOperation::Index { closer, index, .. } => {
                expression = LoweredExpr {
                    kind: LoweredExprKind::Index {
                        collection: Box::new(expression),
                        index: Box::new(lower_expression(&index)?),
                    },
                    span: Span {
                        start,
                        end: span_token(&closer).end,
                    },
                };
            }
            PostfixOperation::Field { name, .. } => {
                let (field, field_span) = checked_ident(&name, "field name")?;
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
                    .map(lower_expression)
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
        }
    }
    Ok(expression)
}

fn collect_postfix_operations(node: &Postfix) -> LowerResult<Vec<PostfixOperation>> {
    let mut operations = Vec::new();
    let brackets = pair_tokens(
        node.l_bracket_tokens().collect(),
        node.r_bracket_tokens().collect(),
        "postfix index delimiters",
        node.syntax(),
    )?;
    let indices = node.indices().collect::<Vec<_>>();
    if brackets.len() != indices.len() {
        return Err(malformed_node("postfix index", node.syntax()));
    }
    operations.extend(
        brackets
            .into_iter()
            .zip(indices)
            .map(|((opener, closer), index)| PostfixOperation::Index {
                opener,
                closer,
                index,
            }),
    );

    let dots = node.dot_tokens().collect::<Vec<_>>();
    let names = node.field_names_tokens().collect::<Vec<_>>();
    if dots.len() != names.len() {
        return Err(malformed_node("postfix field", node.syntax()));
    }
    operations.extend(
        dots.into_iter()
            .zip(names)
            .map(|(dot, name)| PostfixOperation::Field { dot, name }),
    );

    let calls = pair_tokens(
        node.l_paren_tokens().collect(),
        node.r_paren_tokens().collect(),
        "postfix call delimiters",
        node.syntax(),
    )?;
    let arguments = node.arguments().collect::<Vec<_>>();
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
    Ok(operations)
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
    } else if let Some(prompt) = node.prompt_expression() {
        lower_prompt(&prompt)
    } else if let Some(await_block) = node.await_block() {
        lower_await_block(&await_block)
    } else {
        Err(malformed_node("primary expression", node.syntax()))
    }
}

fn lower_literal(node: &Literal) -> LowerResult<LoweredExpr> {
    if let Some(token) = node.int_literal_token() {
        let span = span_token(&token);
        let value = token.text().parse::<i64>().map_err(|_| {
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
            .parse::<f64>()
            .map_err(|_| compiler_error("E0003", "invalid Float literal", span))?;
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
    let opener = required(node.open_backtick_token(), "template opener", node.syntax())?;
    let closer = required(
        node.close_backtick_token(),
        "template closer",
        node.syntax(),
    )?;
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
    Ok(LoweredExpr {
        kind: LoweredExprKind::Template(parts),
        span: Span {
            start: span_token(&opener).start,
            end: span_token(&closer).end,
        },
    })
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
        required_ident(node.variant_name_token(), "variant name", node.syntax())?;
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

fn lower_record_constructor(node: &RecordConstructor) -> LowerResult<LoweredExpr> {
    let (name, _) = required_ident(node.ident_token(), "record name", node.syntax())?;
    Ok(LoweredExpr {
        kind: LoweredExprKind::Record {
            name,
            fields: lower_record_value_fields(node.record_value_fields())?,
        },
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
    Ok(LoweredExpr {
        kind: LoweredExprKind::Record {
            name: name.to_owned(),
            fields,
        },
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
    Ok(LoweredExpr {
        kind: LoweredExprKind::List(
            node.expressions()
                .map(|expression| lower_expression(&expression))
                .collect::<LowerResult<_>>()?,
        ),
        span: span_node(node.syntax()),
    })
}

fn lower_map(node: &MapLiteral) -> LowerResult<LoweredExpr> {
    let keys = node.keys().collect::<Vec<_>>();
    let values = node.values().collect::<Vec<_>>();
    if keys.len() != values.len() {
        return Err(malformed_node("map key/value pairs", node.syntax()));
    }
    let entries = keys
        .iter()
        .zip(values.iter())
        .map(|(key, value)| Ok((lower_expression(key)?, lower_expression(value)?)))
        .collect::<LowerResult<_>>()?;
    Ok(LoweredExpr {
        kind: LoweredExprKind::Map(entries),
        span: span_node(node.syntax()),
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
    let pattern = if node.some_token().is_some() {
        LoweredPattern::Option {
            some: true,
            binding: node
                .binding_name_token()
                .map(|token| checked_ident(&token, "option pattern binding").map(|value| value.0))
                .transpose()?,
        }
    } else if node.ok_token().is_some() || node.err_token().is_some() {
        LoweredPattern::Result {
            ok: node.ok_token().is_some(),
            binding: node
                .binding_name_token()
                .map(|token| checked_ident(&token, "result pattern binding").map(|value| value.0))
                .transpose()?,
        }
    } else if node.none_token().is_some() {
        LoweredPattern::Option {
            some: false,
            binding: None,
        }
    } else if node.underscore_token().is_some() {
        LoweredPattern::Wildcard
    } else if node.true_token().is_some() || node.false_token().is_some() {
        LoweredPattern::Bool(node.true_token().is_some())
    } else if let Some(record) = node.record_pattern() {
        lower_record_pattern(&record)?
    } else if let Some(enumeration) = node.enum_pattern() {
        lower_enum_pattern(&enumeration)?
    } else {
        return Err(malformed_node("pattern alternative", node.syntax()));
    };
    Ok((pattern, span))
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
    let mut binding_tokens = node.binding_names_tokens().collect::<Vec<_>>();
    binding_tokens.extend(node.underscore_tokens());
    binding_tokens.sort_by_key(|token| token.text_range().start());
    let bindings = binding_tokens
        .into_iter()
        .map(|token| {
            if token.kind() == SyntaxKind::Underscore {
                Ok(None)
            } else {
                checked_ident(&token, "enum pattern binding").map(|value| Some(value.0))
            }
        })
        .collect::<LowerResult<_>>()?;
    let fields = node
        .l_brace_token()
        .is_some()
        .then(|| lower_pattern_fields(node.pattern_fields()))
        .transpose()?;
    Ok(LoweredPattern::Enum {
        name,
        variant,
        bindings,
        fields,
    })
}

fn lower_pattern_fields(
    fields: impl IntoIterator<Item = PatternField>,
) -> LowerResult<Vec<(String, Span, Option<String>)>> {
    fields
        .into_iter()
        .map(|field| {
            let (name, span) =
                required_ident(field.field_name_token(), "pattern field", field.syntax())?;
            let binding = if field.colon_token().is_some() {
                field
                    .binding_name_token()
                    .map(|token| {
                        checked_ident(&token, "pattern field binding").map(|value| value.0)
                    })
                    .transpose()?
            } else {
                Some(name.clone())
            };
            Ok((name, span, binding))
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
