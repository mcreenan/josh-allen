//! Canonical checked conversion from lossless typed syntax into compiler state.

use super::{
    Binary, Diagnostic, InlineManifest, LoweredBody, LoweredCallArgument, LoweredConstDeclaration,
    LoweredElse, LoweredEnumPayload, LoweredEnumValuePayload, LoweredEnumVariant, LoweredExpr,
    LoweredExprKind, LoweredForSource, LoweredFunction, LoweredImport, LoweredListItem,
    LoweredLocalFunction, LoweredLoopBinding, LoweredLoopBindingElement, LoweredMapItem,
    LoweredModule, LoweredParameterDefault, LoweredPattern, LoweredStatement, LoweredTemplatePart,
    LoweredType, LoweredTypeDeclaration, SourceWordContext, Span, Unary, is_canonical_effect,
    is_forbidden_source_word,
};
use allen_schema::ToolRequirement;
use allen_syntax::{
    AstNode, Body, ConstDeclaration, Declaration, EffectClause, EnumDeclaration, EnumVariant,
    ForStatement, FunctionDeclaration, FunctionType, GenericType, ImportDeclaration,
    InlineManifest as SyntaxInlineManifest, LocalFunction, LoopBinding, LoopBindingItem, NamedType,
    NewtypeDeclaration, Parse, RecordDeclaration, RecordField, RecordType, Source, SourceFile,
    Statement, SyntaxKind, SyntaxNode, SyntaxToken, TestDeclaration, TupleType, Type,
    TypeAliasDeclaration, WhileStatement,
};
use std::collections::BTreeSet;

#[cfg(test)]
use std::cell::Cell;

use allen_syntax::SourceFileId;

mod expressions;

#[cfg(test)]
mod cutover;

use expressions::{lower_conditional, lower_expression};

#[cfg(test)]
thread_local! {
    static PARSE_INVOCATIONS: Cell<usize> = const { Cell::new(0) };
}

type LowerResult<T> = Result<T, SyntaxLoweringError>;

#[derive(Clone, Debug, Eq, PartialEq)]
enum SyntaxLoweringError {
    SyntaxErrors {
        count: usize,
        first_code: Option<&'static str>,
        span: Span,
    },
    SourceMismatch,
    MalformedTree {
        expected: &'static str,
        span: Span,
    },
    Compiler(Diagnostic),
}

#[derive(Clone, Debug)]
pub(super) struct CheckedSource {
    pub(super) manifest: Option<InlineManifest>,
    pub(super) module: LoweredModule,
}

/// Converts one syntax-clean tree into fully owned frontend state.
///
/// No syntax node or token is retained by the returned semantic structures.
fn lower_checked(source: &SourceFile, parsed: &Parse) -> LowerResult<CheckedSource> {
    if parsed.source_id() != source.id() {
        return Err(SyntaxLoweringError::SourceMismatch);
    }
    let syntax = parsed.syntax();
    if !tree_matches_source(&syntax, source.text()) {
        return Err(SyntaxLoweringError::SourceMismatch);
    }
    if parsed.has_errors() {
        let first = parsed.diagnostics().first();
        return Err(SyntaxLoweringError::SyntaxErrors {
            count: parsed.diagnostics().len(),
            first_code: first.map(allen_syntax::SyntaxDiagnostic::code),
            span: first.map_or_else(
                || span_node(&syntax),
                |diagnostic| span_range(diagnostic.range()),
            ),
        });
    }
    let root = Source::cast(syntax).ok_or_else(|| malformed_node("Source", &parsed.syntax()))?;
    let manifest = root
        .inline_manifest()
        .map(|manifest| lower_manifest(&manifest))
        .transpose()?;
    let imports = root
        .import_declarations()
        .map(|import| lower_import(&import))
        .collect::<LowerResult<Vec<_>>>()?;
    let mut types = Vec::new();
    let mut constants = Vec::new();
    let mut functions = Vec::new();
    let mut tests = Vec::new();
    for declaration in root.declarations() {
        lower_declaration(
            &declaration,
            &mut types,
            &mut constants,
            &mut functions,
            &mut tests,
        )?;
    }
    let mut test_names = BTreeSet::new();
    for test in &tests {
        if !test_names.insert(test.name.clone()) {
            return Err(compiler_error(
                "E3005",
                format!("duplicate test name {:?}", test.name),
                test.name_span,
            ));
        }
    }
    Ok(CheckedSource {
        manifest,
        module: LoweredModule {
            imports,
            types,
            constants,
            functions,
            tests,
        },
    })
}

pub(super) fn parse_module(path: &str, text: &str) -> Result<LoweredModule, Diagnostic> {
    lower_source(path, text).map(|checked| checked.module)
}

pub(super) fn lower_source(path: &str, text: &str) -> Result<CheckedSource, Diagnostic> {
    let (source, parsed) = parse_source(path, text)?;
    lower_checked(&source, &parsed).map_err(|error| lowering_diagnostic(path, &parsed, error))
}

pub(super) fn extract_manifest(
    path: &str,
    text: &str,
) -> Result<Option<InlineManifest>, Diagnostic> {
    let (source, parsed) = parse_source(path, text)?;
    let syntax = parsed.syntax();
    if parsed.source_id() != source.id() || !tree_matches_source(&syntax, source.text()) {
        return Err(Diagnostic::new(
            "E3005",
            "syntax source identity mismatch",
            Span { start: 0, end: 0 },
        )
        .with_source(path));
    }
    let root = Source::cast(syntax.clone()).ok_or_else(|| {
        Diagnostic::new("E3005", "syntax tree is malformed", span_node(&syntax)).with_source(path)
    })?;
    let Some(manifest) = root.inline_manifest() else {
        return Ok(None);
    };
    let manifest_end = manifest.syntax().text_range().end();
    let has_recovery = manifest
        .syntax()
        .descendants_with_tokens()
        .any(|element| match element {
            allen_syntax::SyntaxElement::Node(node) => {
                matches!(node.kind(), SyntaxKind::Error | SyntaxKind::Missing)
            }
            allen_syntax::SyntaxElement::Token(token) => token.kind() == SyntaxKind::ErrorToken,
        });
    let diagnostic = parsed.diagnostics().iter().find(|diagnostic| {
        diagnostic.range().start() < manifest_end
            || (has_recovery && diagnostic.range().start() == manifest_end)
    });
    if has_recovery || diagnostic.is_some() {
        let span = diagnostic.map_or_else(
            || span_node(manifest.syntax()),
            |diagnostic| span_range(diagnostic.range()),
        );
        return Err(syntax_diagnostic(path, diagnostic, span));
    }
    lower_manifest(&manifest)
        .map(Some)
        .map_err(|error| match error {
            SyntaxLoweringError::Compiler(diagnostic) => diagnostic.with_source(path),
            SyntaxLoweringError::MalformedTree { span, .. } => {
                Diagnostic::new("E3005", "syntax tree is malformed", span).with_source(path)
            }
            SyntaxLoweringError::SourceMismatch | SyntaxLoweringError::SyntaxErrors { .. } => {
                Diagnostic::new(
                    "E3005",
                    "syntax tree is malformed",
                    span_node(manifest.syntax()),
                )
                .with_source(path)
            }
        })
}

fn parse_source(path: &str, text: &str) -> Result<(SourceFile, Parse), Diagnostic> {
    let source = SourceFile::new(SourceFileId::new(0), text).map_err(|_| {
        Diagnostic::new(
            "E3005",
            "source exceeds the syntax frontend byte limit",
            Span { start: 0, end: 0 },
        )
        .with_source(path)
    })?;
    #[cfg(test)]
    PARSE_INVOCATIONS.with(|count| count.set(count.get() + 1));
    let parsed = allen_syntax::parse(&source);
    Ok((source, parsed))
}

fn lowering_diagnostic(path: &str, parsed: &Parse, error: SyntaxLoweringError) -> Diagnostic {
    match error {
        SyntaxLoweringError::Compiler(diagnostic) => diagnostic.with_source(path),
        SyntaxLoweringError::SyntaxErrors { span, .. } => {
            syntax_diagnostic(path, parsed.diagnostics().first(), span)
        }
        SyntaxLoweringError::SourceMismatch => Diagnostic::new(
            "E3005",
            "syntax source identity mismatch",
            Span { start: 0, end: 0 },
        )
        .with_source(path),
        SyntaxLoweringError::MalformedTree { span, .. } => {
            Diagnostic::new("E3005", "syntax tree is malformed", span).with_source(path)
        }
    }
}

fn syntax_diagnostic(
    path: &str,
    diagnostic: Option<&allen_syntax::SyntaxDiagnostic>,
    span: Span,
) -> Diagnostic {
    let code = match diagnostic.map(allen_syntax::SyntaxDiagnostic::code) {
        Some("S0002" | "S0003" | "S0004") => "E0004",
        Some("S0005" | "S0006") => "E0005",
        _ => "E3005",
    };
    let message = match diagnostic.map(allen_syntax::SyntaxDiagnostic::code) {
        Some("S0005") => "block comment nesting exceeds the limit of 128".to_owned(),
        Some("S0006") => "unterminated block comment".to_owned(),
        _ => diagnostic.map_or_else(
            || "source contains syntax errors".to_owned(),
            |diagnostic| format!("{} ({})", diagnostic.message(), diagnostic.code()),
        ),
    };
    Diagnostic::new(code, message, span).with_source(path)
}

#[cfg(test)]
pub(super) fn count_parse_invocations<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    PARSE_INVOCATIONS.with(|count| {
        let previous = count.replace(0);
        let result = operation();
        let invocations = count.replace(previous);
        (result, invocations)
    })
}

fn tree_matches_source(root: &SyntaxNode, source: &str) -> bool {
    let mut offset = 0usize;
    for token in root
        .descendants_with_tokens()
        .filter_map(allen_syntax::SyntaxElement::into_token)
        .filter(|token| token.kind() != SyntaxKind::Eof)
    {
        let Some(end) = offset.checked_add(token.text().len()) else {
            return false;
        };
        if source.get(offset..end) != Some(token.text()) {
            return false;
        }
        offset = end;
    }
    offset == source.len()
}

fn lower_declaration(
    declaration: &Declaration,
    types: &mut Vec<LoweredTypeDeclaration>,
    constants: &mut Vec<LoweredConstDeclaration>,
    functions: &mut Vec<LoweredFunction>,
    tests: &mut Vec<super::LoweredTest>,
) -> LowerResult<()> {
    if let Some(record) = declaration.record_declaration() {
        types.push(lower_record_declaration(&record)?);
    } else if let Some(enumeration) = declaration.enum_declaration() {
        types.push(lower_enum_declaration(&enumeration)?);
    } else if let Some(alias) = declaration.type_alias_declaration() {
        types.push(lower_type_alias_declaration(&alias)?);
    } else if let Some(newtype) = declaration.newtype_declaration() {
        types.push(lower_newtype_declaration(&newtype)?);
    } else if let Some(constant) = declaration.const_declaration() {
        constants.push(lower_const_declaration(&constant)?);
    } else if let Some(function) = declaration.function_declaration() {
        functions.push(lower_function(&function)?);
    } else if let Some(test) = declaration.test_declaration() {
        tests.push(lower_test(&test)?);
    } else {
        return Err(malformed_node(
            "declaration alternative",
            declaration.syntax(),
        ));
    }
    Ok(())
}

fn lower_test(node: &TestDeclaration) -> LowerResult<super::LoweredTest> {
    let token = required(node.test_name_token(), "test name", node.syntax())?;
    let name_span = span_token(&token);
    let name = decode_string(token.text(), name_span)?;
    let (declared_effects, effects_span) = lower_optional_effects(node.effect_clause())?;
    Ok(super::LoweredTest {
        name,
        name_span,
        offset: usize::from(node.syntax().text_range().start()),
        declared_effects: declared_effects.unwrap_or_default(),
        effects_span,
        body: lower_body(&required(node.body(), "test body", node.syntax())?)?,
    })
}

fn lower_const_declaration(node: &ConstDeclaration) -> LowerResult<LoweredConstDeclaration> {
    let (name, name_span) = required_ident(node.ident_token(), "constant name", node.syntax())?;
    let value_type = lower_type(&required(
        node.ty(),
        "constant declared type",
        node.syntax(),
    )?)?;
    let value = lower_expression(&required(
        node.expression(),
        "constant value",
        node.syntax(),
    )?)?;
    Ok(LoweredConstDeclaration {
        exported: node.export_token().is_some(),
        name,
        name_span,
        value_type,
        value,
    })
}

fn lower_newtype_declaration(node: &NewtypeDeclaration) -> LowerResult<LoweredTypeDeclaration> {
    let (name, name_span) = required_ident(node.ident_token(), "newtype name", node.syntax())?;
    let underlying = lower_type(&required(
        node.ty(),
        "newtype underlying type",
        node.syntax(),
    )?)?;
    Ok(LoweredTypeDeclaration::Newtype {
        exported: node.export_token().is_some(),
        name,
        name_span,
        underlying,
    })
}

fn lower_type_alias_declaration(
    node: &TypeAliasDeclaration,
) -> LowerResult<LoweredTypeDeclaration> {
    let (name, name_span) = required_ident(node.ident_token(), "type alias name", node.syntax())?;
    let target = lower_type(&required(node.ty(), "type alias target", node.syntax())?)?;
    Ok(LoweredTypeDeclaration::Alias {
        exported: node.export_token().is_some(),
        name,
        name_span,
        target,
    })
}

fn lower_record_declaration(node: &RecordDeclaration) -> LowerResult<LoweredTypeDeclaration> {
    let (name, name_span) = required_ident(node.ident_token(), "record name", node.syntax())?;
    Ok(LoweredTypeDeclaration::Record {
        exported: node.export_token().is_some(),
        name,
        name_span,
        fields: node
            .record_fields()
            .map(|field| lower_record_field(&field))
            .collect::<LowerResult<_>>()?,
        invariant: node
            .predicate()
            .map(|predicate| lower_expression(&predicate))
            .transpose()?,
    })
}

fn lower_enum_declaration(node: &EnumDeclaration) -> LowerResult<LoweredTypeDeclaration> {
    let (name, name_span) = required_ident(node.ident_token(), "enum name", node.syntax())?;
    let variants = node
        .enum_variants()
        .map(|variant| lower_enum_variant(&variant))
        .collect::<LowerResult<Vec<_>>>()?;
    if variants.is_empty() {
        return Err(compiler_error(
            "E3005",
            "enum requires at least one variant",
            name_span,
        ));
    }
    Ok(LoweredTypeDeclaration::Enum {
        exported: node.export_token().is_some(),
        name,
        name_span,
        variants,
    })
}

fn lower_enum_variant(node: &EnumVariant) -> LowerResult<LoweredEnumVariant> {
    let (name, span) = required_ident(node.ident_token(), "enum variant name", node.syntax())?;
    let payload = if node.l_paren_token().is_some() {
        let values = node
            .types()
            .map(|ty| lower_type(&ty))
            .collect::<LowerResult<Vec<_>>>()?;
        if values.is_empty() {
            return Err(compiler_error(
                "E3005",
                "tuple enum variant requires at least one payload type",
                span,
            ));
        }
        LoweredEnumPayload::Tuple(values)
    } else if node.l_brace_token().is_some() {
        LoweredEnumPayload::Record(
            node.record_fields()
                .map(|field| lower_record_field(&field))
                .collect::<LowerResult<_>>()?,
        )
    } else {
        LoweredEnumPayload::Unit
    };
    Ok(LoweredEnumVariant {
        name,
        span,
        payload,
    })
}

fn lower_function(node: &FunctionDeclaration) -> LowerResult<LoweredFunction> {
    let (name, name_span) = required_ident(node.ident_token(), "function name", node.syntax())?;
    let generics = node.generic_parameters().map_or_else(
        || Ok(Vec::new()),
        |parameters| {
            parameters
                .generic_parameters()
                .map(|parameter| {
                    required_ident(
                        parameter.ident_token(),
                        "generic parameter",
                        parameter.syntax(),
                    )
                })
                .collect()
        },
    )?;
    let lowered_parameters = node
        .parameters()
        .map(|parameter| lower_parameter_with_default(&parameter))
        .collect::<LowerResult<Vec<_>>>()?;
    let mut parameters = Vec::with_capacity(lowered_parameters.len());
    let mut parameter_defaults = Vec::with_capacity(lowered_parameters.len());
    for (parameter, default) in lowered_parameters {
        parameters.push(parameter);
        parameter_defaults.push(default);
    }
    let return_type = lower_type(&required(node.ty(), "function return type", node.syntax())?)?;
    let (declared_effects, effects_span) = lower_optional_effects(node.effect_clause())?;
    let body = lower_body(&required(node.body(), "function body", node.syntax())?)?;
    Ok(LoweredFunction {
        exported: node.export_token().is_some(),
        is_async: node.async_token().is_some(),
        name,
        name_span,
        generics,
        parameters,
        parameter_defaults,
        return_type,
        declared_effects: Some(declared_effects.unwrap_or_default()),
        effects_span,
        body,
    })
}

fn lower_parameter(node: &allen_syntax::Parameter) -> LowerResult<(String, LoweredType, Span)> {
    let (name, span) = required_ident(node.ident_token(), "parameter name", node.syntax())?;
    let ty = lower_type(&required(node.ty(), "parameter type", node.syntax())?)?;
    Ok((name, ty, span))
}

fn lower_parameter_with_default(
    node: &allen_syntax::Parameter,
) -> LowerResult<((String, LoweredType, Span), Option<LoweredParameterDefault>)> {
    let parameter = lower_parameter(node)?;
    let default = node
        .default_value()
        .map(|value| {
            let span = span_node(value.syntax());
            Ok(LoweredParameterDefault {
                value: lower_expression(&value)?,
                source_text: value.syntax().text().to_string(),
                span,
            })
        })
        .transpose()?;
    Ok((parameter, default))
}

fn lower_record_field(node: &RecordField) -> LowerResult<(String, LoweredType, Span)> {
    let (name, span) = required_ident(node.ident_token(), "record field name", node.syntax())?;
    let ty = lower_type(&required(node.ty(), "record field type", node.syntax())?)?;
    Ok((name, ty, span))
}

fn lower_import(node: &ImportDeclaration) -> LowerResult<LoweredImport> {
    let names = node
        .import_names()
        .map(|name| {
            let (imported, imported_span) =
                required_ident(name.imported_name_token(), "imported name", name.syntax())?;
            let (local, local_span) = name.local_name_token().map_or_else(
                || Ok((imported.clone(), imported_span)),
                |token| checked_ident(&token, "local import name"),
            )?;
            Ok((imported, local, local_span))
        })
        .collect::<LowerResult<_>>()?;
    let path_token = required(node.import_source_token(), "import source", node.syntax())?;
    let path = decode_string(path_token.text(), span_token(&path_token))?;
    Ok(LoweredImport {
        extension: node.extension_keyword_token().is_some(),
        names,
        path,
        resolved_path: None,
        span: span_node(node.syntax()),
    })
}

fn lower_type(node: &Type) -> LowerResult<LoweredType> {
    if let Some(named) = node.named_type() {
        lower_named_type(&named)
    } else if let Some(generic) = node.generic_type() {
        lower_generic_type(&generic)
    } else if let Some(tuple) = node.tuple_type() {
        lower_tuple_type(&tuple)
    } else if let Some(record) = node.record_type() {
        lower_record_type(&record)
    } else if let Some(function) = node.function_type() {
        lower_function_type(&function)
    } else {
        Err(malformed_node("type alternative", node.syntax()))
    }
}

fn lower_named_type(node: &NamedType) -> LowerResult<LoweredType> {
    let segments = node.segments_tokens().collect::<Vec<_>>();
    let first = required(segments.first().cloned(), "named type", node.syntax())?;
    let name = segments
        .iter()
        .map(SyntaxToken::text)
        .collect::<Vec<_>>()
        .join(".");
    let span = Span {
        start: span_token(&first).start,
        end: span_token(segments.last().expect("required first named type segment")).end,
    };
    Ok(LoweredType::Named(name, span))
}

fn lower_generic_type(node: &GenericType) -> LowerResult<LoweredType> {
    let span = span_node(node.syntax());
    if node.list_token().is_some() {
        Ok(LoweredType::List(
            Box::new(lower_type(&required(
                node.type_argument(),
                "List type argument",
                node.syntax(),
            )?)?),
            span,
        ))
    } else if node.option_token().is_some() {
        Ok(LoweredType::Option(
            Box::new(lower_type(&required(
                node.type_argument(),
                "Option type argument",
                node.syntax(),
            )?)?),
            span,
        ))
    } else if node.future_token().is_some() {
        Ok(LoweredType::Future(
            Box::new(lower_type(&required(
                node.type_argument(),
                "Future type argument",
                node.syntax(),
            )?)?),
            span,
        ))
    } else if node.task_token().is_some() {
        Ok(LoweredType::Task(
            Box::new(lower_type(&required(
                node.type_argument(),
                "Task type argument",
                node.syntax(),
            )?)?),
            span,
        ))
    } else if node.prompt_type_token().is_some() {
        Ok(LoweredType::Prompt(
            Box::new(lower_type(&required(
                node.type_argument(),
                "Prompt type argument",
                node.syntax(),
            )?)?),
            span,
        ))
    } else if node.range_token().is_some() {
        Ok(LoweredType::Range(
            Box::new(lower_type(&required(
                node.type_argument(),
                "Range type argument",
                node.syntax(),
            )?)?),
            span,
        ))
    } else if node.sequence_token().is_some() {
        Ok(LoweredType::Sequence(
            Box::new(lower_type(&required(
                node.type_argument(),
                "Sequence type argument",
                node.syntax(),
            )?)?),
            span,
        ))
    } else if node.map_type_token().is_some() {
        Ok(LoweredType::Map(
            Box::new(lower_type(&required(
                node.first_type_argument(),
                "Map key type",
                node.syntax(),
            )?)?),
            Box::new(lower_type(&required(
                node.second_type_argument(),
                "Map value type",
                node.syntax(),
            )?)?),
            span,
        ))
    } else if node.result_token().is_some() {
        Ok(LoweredType::Result(
            Box::new(lower_type(&required(
                node.first_type_argument(),
                "Result success type",
                node.syntax(),
            )?)?),
            Box::new(lower_type(&required(
                node.second_type_argument(),
                "Result error type",
                node.syntax(),
            )?)?),
            span,
        ))
    } else {
        Err(malformed_node("generic type constructor", node.syntax()))
    }
}

fn lower_tuple_type(node: &TupleType) -> LowerResult<LoweredType> {
    Ok(LoweredType::Tuple(
        node.types()
            .map(|ty| lower_type(&ty))
            .collect::<LowerResult<_>>()?,
        span_node(node.syntax()),
    ))
}

fn lower_record_type(node: &RecordType) -> LowerResult<LoweredType> {
    Ok(LoweredType::Record(
        node.record_fields()
            .map(|field| lower_record_field(&field))
            .collect::<LowerResult<_>>()?,
        span_node(node.syntax()),
    ))
}

fn lower_function_type(node: &FunctionType) -> LowerResult<LoweredType> {
    let parameters = node
        .parameter_types()
        .map(|ty| lower_type(&ty))
        .collect::<LowerResult<_>>()?;
    let return_type = Box::new(lower_type(&required(
        node.return_type(),
        "function type return",
        node.syntax(),
    )?)?);
    let (effects, _) = lower_optional_effects(node.effect_clause())?;
    Ok(LoweredType::Function {
        parameters,
        return_type,
        effects: effects.unwrap_or_default(),
        span: span_node(node.syntax()),
    })
}

fn lower_optional_effects(
    clause: Option<EffectClause>,
) -> LowerResult<(Option<Vec<String>>, Option<Span>)> {
    let Some(clause) = clause else {
        return Ok((None, None));
    };
    let span = span_node(clause.syntax());
    let mut effects = Vec::new();
    for token in clause.effect_id_tokens() {
        let effect = token.text().to_owned();
        if !is_canonical_effect(&effect) {
            return Err(compiler_error(
                "E2403",
                format!("invalid effect ID '{effect}'"),
                span_token(&token),
            ));
        }
        effects.push(effect);
    }
    effects.sort();
    effects.dedup();
    Ok((Some(effects), Some(span)))
}

fn lower_body(node: &Body) -> LowerResult<LoweredBody> {
    Ok(LoweredBody {
        statements: node
            .statements()
            .map(|statement| lower_statement(&statement))
            .collect::<LowerResult<_>>()?,
        tail: node
            .expression()
            .map(|expression| lower_expression(&expression))
            .transpose()?,
        span: span_node(node.syntax()),
    })
}

fn lower_statement(node: &Statement) -> LowerResult<LoweredStatement> {
    if node.let_token().is_some() || node.mut_token().is_some() {
        let (name, name_span) = required_ident(
            node.binding_name_token(),
            "local binding name",
            node.syntax(),
        )?;
        return Ok(LoweredStatement::Let {
            name,
            name_span,
            mutable: node.mut_token().is_some(),
            annotation: node.ty().map(|ty| lower_type(&ty)).transpose()?,
            value: lower_expression(&required(
                node.initializer(),
                "local initializer",
                node.syntax(),
            )?)?,
        });
    }
    if node.assignment_target_token().is_some() {
        let (name, name_span) = required_ident(
            node.assignment_target_token(),
            "assignment target",
            node.syntax(),
        )?;
        let operation = if node.plus_eq_token().is_some() {
            Some(Binary::Add)
        } else if node.minus_eq_token().is_some() {
            Some(Binary::Subtract)
        } else if node.star_eq_token().is_some() {
            Some(Binary::Multiply)
        } else if node.slash_eq_token().is_some() {
            Some(Binary::Divide)
        } else if node.percent_eq_token().is_some() {
            Some(Binary::Remainder)
        } else {
            None
        };
        return Ok(LoweredStatement::Assignment {
            name,
            name_span,
            operation,
            value: lower_expression(&required(
                node.assignment_value(),
                "assignment value",
                node.syntax(),
            )?)?,
        });
    }
    if node.return_token().is_some() {
        return Ok(LoweredStatement::Return(
            node.return_value()
                .map(|value| lower_expression(&value))
                .transpose()?,
            span_node(node.syntax()),
        ));
    }
    if let Some(conditional) = node.conditional_expression() {
        return Ok(LoweredStatement::ControlFlow(lower_conditional(
            &conditional,
        )?));
    }
    if let Some(statement) = node.while_statement() {
        return lower_while(&statement);
    }
    if let Some(statement) = node.loop_statement() {
        let body = lower_body(&required(
            statement.body(),
            "loop body",
            statement.syntax(),
        )?)?;
        return Ok(LoweredStatement::Loop {
            span: span_node(statement.syntax()),
            body,
        });
    }
    if let Some(statement) = node.for_statement() {
        return lower_for(&statement);
    }
    if let Some(function) = node.local_function() {
        return Ok(LoweredStatement::LocalFunction(lower_local_function(
            &function,
        )?));
    }
    if node.break_token().is_some() {
        return Ok(LoweredStatement::Break(span_node(node.syntax())));
    }
    if node.continue_token().is_some() {
        return Ok(LoweredStatement::Continue(span_node(node.syntax())));
    }
    Err(malformed_node("statement alternative", node.syntax()))
}

fn lower_while(node: &WhileStatement) -> LowerResult<LoweredStatement> {
    Ok(LoweredStatement::While {
        condition: lower_expression(&required(
            node.expression(),
            "while condition",
            node.syntax(),
        )?)?,
        body: lower_body(&required(node.body(), "while body", node.syntax())?)?,
        span: span_node(node.syntax()),
    })
}

fn lower_for(node: &ForStatement) -> LowerResult<LoweredStatement> {
    let source = LoweredForSource::Iterable(lower_expression(&required(
        node.iterable(),
        "for source",
        node.syntax(),
    )?)?);
    Ok(LoweredStatement::For {
        binding: lower_loop_binding(&required(
            node.loop_binding(),
            "for binding",
            node.syntax(),
        )?)?,
        source,
        body: lower_body(&required(node.body(), "for body", node.syntax())?)?,
        span: span_node(node.syntax()),
    })
}

fn lower_local_function(node: &LocalFunction) -> LowerResult<LoweredLocalFunction> {
    let (name, name_span) =
        required_ident(node.ident_token(), "local function name", node.syntax())?;
    let lowered_parameters = node
        .parameters()
        .map(|parameter| lower_parameter_with_default(&parameter))
        .collect::<LowerResult<Vec<_>>>()?;
    let mut parameters = Vec::with_capacity(lowered_parameters.len());
    let mut parameter_defaults = Vec::with_capacity(lowered_parameters.len());
    for (parameter, default) in lowered_parameters {
        parameters.push(parameter);
        parameter_defaults.push(default);
    }
    let (declared_effects, effects_span) = lower_optional_effects(node.effect_clause())?;
    Ok(LoweredLocalFunction {
        name,
        name_span,
        parameters,
        parameter_defaults,
        return_type: lower_type(&required(
            node.ty(),
            "local function return type",
            node.syntax(),
        )?)?,
        declared_effects,
        effects_span,
        body: lower_body(&required(
            node.body(),
            "local function body",
            node.syntax(),
        )?)?,
        span: span_node(node.syntax()),
    })
}

fn lower_loop_binding(node: &LoopBinding) -> LowerResult<LoweredLoopBinding> {
    let tuple = node.l_paren_token().is_some();
    let elements = if tuple {
        node.loop_binding_items()
            .map(|item| lower_loop_binding_item(&item))
            .collect::<LowerResult<Vec<_>>>()?
    } else {
        vec![lower_binding_tokens(
            node.ident_token(),
            node.underscore_token(),
            "loop binding",
            node.syntax(),
        )?]
    };
    let mut names = BTreeSet::new();
    for element in &elements {
        if let Some(name) = &element.name {
            if !names.insert(name.clone()) {
                return Err(compiler_error(
                    "E3005",
                    format!("duplicate loop binding '{name}'"),
                    element.span,
                ));
            }
        }
    }
    Ok(LoweredLoopBinding {
        elements,
        tuple,
        span: span_node(node.syntax()),
    })
}

fn lower_loop_binding_item(node: &LoopBindingItem) -> LowerResult<LoweredLoopBindingElement> {
    lower_binding_tokens(
        node.ident_token(),
        node.underscore_token(),
        "loop binding item",
        node.syntax(),
    )
}

fn lower_binding_tokens(
    ident: Option<SyntaxToken>,
    underscore: Option<SyntaxToken>,
    expected: &'static str,
    owner: &SyntaxNode,
) -> LowerResult<LoweredLoopBindingElement> {
    let token = ident
        .or(underscore)
        .ok_or_else(|| malformed_node(expected, owner))?;
    let span = span_token(&token);
    let name = if token.kind() == SyntaxKind::Underscore {
        None
    } else {
        Some(checked_ident(&token, expected)?.0)
    };
    Ok(LoweredLoopBindingElement { name, span })
}

fn lower_manifest(node: &SyntaxInlineManifest) -> LowerResult<InlineManifest> {
    let mut language = None;
    let mut entry = None;
    let mut capabilities = None;
    let mut http_origins = None;
    let mut tools = None;
    for field in node.manifest_fields() {
        if let Some(keyword) = field.language_token() {
            reject_duplicate(language.as_ref(), "language", span_token(&keyword))?;
            let token = required(
                field.language_value_token(),
                "manifest language value",
                field.syntax(),
            )?;
            language = Some(decode_string(token.text(), span_token(&token))?);
        } else if let Some(keyword) = field.entry_token() {
            reject_duplicate(entry.as_ref(), "entry", span_token(&keyword))?;
            entry =
                Some(required_ident(field.entry_name_token(), "manifest entry", field.syntax())?.0);
        } else if let Some(keyword) = field.capabilities_token() {
            reject_duplicate(capabilities.as_ref(), "capabilities", span_token(&keyword))?;
            let mut values = field
                .capabilities()
                .map(|capability| {
                    let effect = required(
                        capability.effect_id_token(),
                        "manifest capability effect",
                        capability.syntax(),
                    )?;
                    if !is_canonical_effect(effect.text()) {
                        return Err(compiler_error(
                            "E2403",
                            format!("invalid capability '{}'", effect.text()),
                            span_token(&effect),
                        ));
                    }
                    let mut value = effect.text().to_owned();
                    if let Some(argument) = capability.ident_token() {
                        let (argument, _) =
                            checked_ident(&argument, "manifest capability argument")?;
                        value.push('(');
                        value.push_str(&argument);
                        value.push(')');
                    }
                    Ok(value)
                })
                .collect::<LowerResult<Vec<_>>>()?;
            values.sort();
            reject_adjacent_duplicate(
                &values,
                "inline manifest repeats a capability",
                span_token(&keyword),
            )?;
            capabilities = Some(values);
        } else if let Some(keyword) = field.http_origins_token() {
            reject_duplicate(http_origins.as_ref(), "http_origins", span_token(&keyword))?;
            let mut values = field
                .http_origins_tokens()
                .map(|token| decode_string(token.text(), span_token(&token)))
                .collect::<LowerResult<Vec<_>>>()?;
            values.sort();
            reject_adjacent_duplicate(
                &values,
                "inline manifest repeats an HTTP origin",
                span_token(&keyword),
            )?;
            http_origins = Some(values);
        } else if let Some(keyword) = field.tools_token() {
            reject_duplicate(tools.as_ref(), "tools", span_token(&keyword))?;
            let mut values = field
                .tool_requirements()
                .map(|requirement| lower_tool_requirement(&requirement))
                .collect::<LowerResult<Vec<_>>>()?;
            values.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
            if values.windows(2).any(|pair| pair[0].name == pair[1].name) {
                return Err(compiler_error(
                    "E3005",
                    "inline tools repeats a required name",
                    span_token(&keyword),
                ));
            }
            tools = Some(values);
        } else {
            return Err(malformed_node("manifest field alternative", field.syntax()));
        }
    }
    let span = span_node(node.syntax());
    Ok(InlineManifest {
        language: language
            .ok_or_else(|| compiler_error("E3005", "inline manifest requires language", span))?,
        entry: entry
            .ok_or_else(|| compiler_error("E3005", "inline manifest requires entry", span))?,
        capabilities: capabilities.ok_or_else(|| {
            compiler_error("E3005", "inline manifest requires capabilities", span)
        })?,
        http_origins: http_origins.unwrap_or_default(),
        tools: tools.unwrap_or_default(),
    })
}

fn lower_tool_requirement(node: &allen_syntax::ToolRequirement) -> LowerResult<ToolRequirement> {
    let name_token = required(node.tool_name_token(), "tool name", node.syntax())?;
    let version_token = required(node.tool_version_token(), "tool version", node.syntax())?;
    let name = decode_string(name_token.text(), span_token(&name_token))?;
    let version = decode_string(version_token.text(), span_token(&version_token))?;
    ToolRequirement::parse(&name, &version).map_err(|_| {
        compiler_error(
            "E3005",
            "inline tool name or version is not canonical",
            span_node(node.syntax()),
        )
    })
}

fn reject_duplicate<T>(value: Option<&T>, field: &str, span: Span) -> LowerResult<()> {
    if value.is_some() {
        return Err(compiler_error(
            "E3005",
            format!("inline manifest defines {field} more than once"),
            span,
        ));
    }
    Ok(())
}

fn reject_adjacent_duplicate<T: PartialEq>(
    values: &[T],
    message: &'static str,
    span: Span,
) -> LowerResult<()> {
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(compiler_error("E3005", message, span));
    }
    Ok(())
}

fn required<T>(value: Option<T>, expected: &'static str, owner: &SyntaxNode) -> LowerResult<T> {
    value.ok_or_else(|| malformed_node(expected, owner))
}

fn required_ident(
    token: Option<SyntaxToken>,
    expected: &'static str,
    owner: &SyntaxNode,
) -> LowerResult<(String, Span)> {
    let token = token.ok_or_else(|| malformed_node(expected, owner))?;
    checked_ident(&token, expected)
}

fn checked_ident(token: &SyntaxToken, expected: &'static str) -> LowerResult<(String, Span)> {
    if token.kind() != SyntaxKind::Ident {
        return Err(SyntaxLoweringError::MalformedTree {
            expected,
            span: span_token(token),
        });
    }
    let name = token.text().to_owned();
    if is_forbidden_source_word(&name, SourceWordContext::Identifier) {
        return Err(compiler_error(
            "E2020",
            format!("'{name}' is forbidden; use Option<T> or unknown"),
            span_token(token),
        ));
    }
    Ok((name, span_token(token)))
}

fn decode_string(text: &str, span: Span) -> LowerResult<String> {
    if let Some(raw) = decode_raw_string(text) {
        return Ok(raw.to_owned());
    }
    let body = text
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| compiler_error("E0004", "malformed string literal", span))?;
    let mut value = String::new();
    let mut characters = body.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            value.push(character);
            continue;
        }
        let escape = characters
            .next()
            .ok_or_else(|| compiler_error("E0004", "unterminated string escape", span))?;
        value.push(char::from(decode_simple_escape(escape, span)?));
    }
    Ok(value)
}

fn decode_raw_string(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('r')?;
    let hashes = rest.bytes().take_while(|byte| *byte == b'#').count();
    let body = rest.strip_prefix(&"#".repeat(hashes))?.strip_prefix('"')?;
    body.strip_suffix(&format!("\"{}", "#".repeat(hashes)))
}

fn decode_bytes(text: &str, span: Span) -> LowerResult<Vec<u8>> {
    let body = text
        .strip_prefix("b\"")
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| compiler_error("E0004", "malformed bytes literal", span))?;
    let bytes = body.as_bytes();
    let mut value = Vec::new();
    let mut position = 0usize;
    while position < bytes.len() {
        if bytes[position] != b'\\' {
            value.push(bytes[position]);
            position += 1;
            continue;
        }
        position += 1;
        let escape = *bytes
            .get(position)
            .ok_or_else(|| compiler_error("E0004", "unterminated bytes escape", span))?;
        position += 1;
        if escape == b'x' {
            let high = bytes
                .get(position)
                .and_then(|byte| hex_digit(*byte))
                .ok_or_else(|| compiler_error("E0004", "invalid byte escape", span))?;
            let low = bytes
                .get(position + 1)
                .and_then(|byte| hex_digit(*byte))
                .ok_or_else(|| compiler_error("E0004", "invalid byte escape", span))?;
            value.push(high * 16 + low);
            position += 2;
        } else {
            value.push(decode_simple_escape(char::from(escape), span)?);
        }
    }
    Ok(value)
}

fn decode_simple_escape(escape: char, span: Span) -> LowerResult<u8> {
    match escape {
        '"' => Ok(b'"'),
        '\\' => Ok(b'\\'),
        'n' => Ok(b'\n'),
        'r' => Ok(b'\r'),
        't' => Ok(b'\t'),
        '0' => Ok(b'\0'),
        'b' => Ok(0x08),
        'f' => Ok(0x0c),
        _ => Err(compiler_error("E0004", "unsupported escape", span)),
    }
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn malformed_node(expected: &'static str, node: &SyntaxNode) -> SyntaxLoweringError {
    SyntaxLoweringError::MalformedTree {
        expected,
        span: span_node(node),
    }
}

fn compiler_error(
    code: &'static str,
    message: impl Into<String>,
    span: Span,
) -> SyntaxLoweringError {
    SyntaxLoweringError::Compiler(Diagnostic::new(code, message, span))
}

fn span_node(node: &SyntaxNode) -> Span {
    let mut tokens = node
        .descendants_with_tokens()
        .filter_map(allen_syntax::SyntaxElement::into_token)
        .filter(|token| {
            !matches!(
                token.kind(),
                SyntaxKind::Whitespace
                    | SyntaxKind::Newline
                    | SyntaxKind::LineComment
                    | SyntaxKind::BlockComment
                    | SyntaxKind::Eof
            )
        });
    let Some(first) = tokens.next() else {
        return span_range(node.text_range());
    };
    let start = first.text_range().start();
    let end = tokens
        .last()
        .map_or(first.text_range().end(), |token| token.text_range().end());
    span_range(allen_syntax::TextRange::new(start, end))
}

fn span_token(token: &SyntaxToken) -> Span {
    span_range(token.text_range())
}

fn span_range(range: allen_syntax::TextRange) -> Span {
    Span {
        start: u32::from(range.start()) as usize,
        end: u32::from(range.end()) as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use allen_syntax::{SourceFileId, SyntaxLimits, parse, parse_with_limits};

    fn syntax_source(text: &str) -> SourceFile {
        SourceFile::new(SourceFileId::new(0), text).expect("test source range")
    }

    fn assert_lowers_deterministically(source: &str) {
        let file = syntax_source(source);
        let parsed = parse(&file);
        assert!(
            !parsed.has_errors(),
            "syntax diagnostics: {:?}",
            parsed.diagnostics()
        );
        let checked = lower_checked(&file, &parsed).expect("checked syntax lowering");
        let repeated = lower_checked(&file, &parsed).expect("repeat checked syntax lowering");
        assert_eq!(checked.manifest, repeated.manifest);
        assert_eq!(
            format!("{:#?}", checked.module),
            format!("{:#?}", repeated.module)
        );
    }

    fn checked_compiler_error(source: &str) -> Diagnostic {
        let file = syntax_source(source);
        let parsed = parse(&file);
        assert!(
            !parsed.has_errors(),
            "syntax diagnostics: {:?}",
            parsed.diagnostics()
        );
        match lower_checked(&file, &parsed).expect_err("lowering must reject the source") {
            SyntaxLoweringError::Compiler(diagnostic) => diagnostic,
            other => panic!("expected compiler diagnostic, got {other:?}"),
        }
    }

    fn assert_compiler_error_is_deterministic(source: &str) {
        assert_eq!(
            checked_compiler_error(source),
            checked_compiler_error(source)
        );
    }

    #[test]
    fn declarations_and_types_match_the_existing_lowered_contract() {
        assert_lowers_deterministically(
            r#"import { A, B as C } from "./dep.allen";
export record Pair { left: Int, right: Option<String> }
export enum Choice { Empty, One(Int), Named { value: Map<String, Int> } }
export async fn convert<T: Eq>(value: T, callback: fn(T) returns Result<Int, String> effects [io.read])
  returns (Int, String) effects [task.spawn, io.read, io.read] {
  return (1, "ok");
}
"#,
        );
    }

    #[test]
    fn statements_expressions_and_patterns_match_the_existing_lowered_contract() {
        assert_lowers_deterministically(
            r"fn run(items: List<Int>, flag: Bool) returns Int effects [] {
  mut total: Int = 0;
  total += 1 + 2 * 3;
  while (flag) { break; }
  loop { continue; }
  for (item, _) in 0..3 { total = total + item; }
  if (flag) { total = total + items[0]; }
  match Some(total) {
    Some(value) => value,
    None => 0,
    Result.Ok(binding) => binding,
    Result.Err(_) => 0,
    Pair { left, right: alias } => left,
    Choice.Named { value } => value,
  }
}
",
        );
    }

    #[test]
    fn literals_postfix_templates_closures_and_prompts_match() {
        assert_lowers_deterministically(
            r#"fn rich(value: Int) returns Int effects [] {
  let bytes = b"a\x00\n";
  let text = `before ${value + 1} after`;
  let list = [1, 2, 3];
  let mapping = map { "a": 1, "b": 2 };
  let item_record = Pair { left: list[0], right: value };
  let variant_value = Choice.Named { value };
  let callback_value = fn(item: Int) returns Int effects [] { item + 1 };
  let request_value = prompt {
    system: text,
    context: "context",
    data: item_record,
    output: Int,
    policy: { max_attempts: 2 },
  };
  callback_value(item_record.left)?
}
"#,
        );
    }

    #[test]
    fn escaped_template_backtick_decodes_deterministically() {
        assert_lowers_deterministically(
            r"fn main() returns String effects [] { `before \` after` }
",
        );
    }

    #[test]
    fn l1_literals_labeled_calls_and_short_closures_reach_lowering() {
        assert_lowers_deterministically(
            r####"fn main() returns String effects [] {
  let raw = r###"${name} \\d+ "###;
  let message = """
    before ${raw}\n
    after
    """;
  let reordered = render(value: message, source: raw);
  let callback = fn(value) => value;
  callback(reordered)
}
"####,
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn l2_forms_reach_neutral_lowering_with_source_order_and_spans() {
        let source = r#"import extension { render as show } from "ui";
record User { active: Bool }
fn configured(count: Int = 3, delay: Int = count) returns Int { count + delay }
fn main() returns Void {
  let updated = User { ..user, active: true };
  let values = [..prefix, item, ..suffix];
  let settings = map { ..defaults, "mode": selected };
  let partial = log.write(level: "warn", message: _);
  let folded = list.fold(values, 0) fn(total, value) => total + value;
  let chained = maybe?.user?.show();
  let piped = input |> normalize >> render;
  ()
}
"#;
        let checked = lower_source("l2.allen", source).expect("L2 syntax lowers");
        assert!(checked.module.imports[0].extension);

        let configured = checked
            .module
            .functions
            .iter()
            .find(|function| function.name == "configured")
            .expect("configured function");
        assert_eq!(configured.parameter_defaults.len(), 2);
        assert_eq!(
            configured.parameter_defaults[0]
                .as_ref()
                .expect("count default")
                .source_text,
            " 3"
        );
        assert_eq!(
            configured.parameter_defaults[1]
                .as_ref()
                .expect("delay default")
                .source_text,
            " count"
        );

        let main = checked
            .module
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main function");
        let binding = |name: &str| {
            main.body
                .statements
                .iter()
                .find_map(|statement| match statement {
                    LoweredStatement::Let {
                        name: binding,
                        value,
                        ..
                    } if binding == name => Some(value),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing `{name}` binding"))
        };

        assert!(matches!(
            &binding("updated").kind,
            LoweredExprKind::RecordUpdate { .. }
        ));
        let LoweredExprKind::ListWithSpread(items) = &binding("values").kind else {
            panic!("expected list spread carrier");
        };
        assert_eq!(
            items.iter().map(|item| item.spread).collect::<Vec<_>>(),
            [true, false, true]
        );
        assert!(items.iter().all(|item| item.span.start < item.span.end));

        let LoweredExprKind::MapWithSpread(items) = &binding("settings").kind else {
            panic!("expected map spread carrier");
        };
        assert!(matches!(items[0], LoweredMapItem::Spread { .. }));
        assert!(matches!(items[1], LoweredMapItem::Entry { .. }));

        let LoweredExprKind::Call { arguments, .. } = &binding("partial").kind else {
            panic!("expected partial call carrier");
        };
        assert!(!arguments[0].placeholder);
        assert!(arguments[1].placeholder);
        assert!(!arguments[1].trailing);

        let LoweredExprKind::Call { arguments, .. } = &binding("folded").kind else {
            panic!("expected trailing callback call carrier");
        };
        let callback = arguments.last().expect("trailing callback");
        assert!(callback.trailing);
        assert!(callback.preceding_call_span.is_some());
        assert!(matches!(
            &callback.value.kind,
            LoweredExprKind::ShortClosure { .. }
        ));

        let LoweredExprKind::Call { callee, .. } = &binding("chained").kind else {
            panic!("expected chained call carrier");
        };
        let LoweredExprKind::OptionalFieldGet { receiver, .. } = &callee.kind else {
            panic!("expected optional call member");
        };
        assert!(matches!(
            &receiver.kind,
            LoweredExprKind::OptionalFieldGet { .. }
        ));

        let LoweredExprKind::Pipe { stage, .. } = &binding("piped").kind else {
            panic!("expected pipe carrier");
        };
        assert!(matches!(&stage.kind, LoweredExprKind::Compose { .. }));
    }

    #[test]
    fn l3_forms_reach_neutral_lowering_with_operator_and_container_spans() {
        let source = "fn outer(values: List<Int>, subject: T) returns Sequence<Int> { fn local(limit: Int) returns Range<Int> { 0..=limit } let window = values[1..3]; let first = values[0]; match subject { 1..=3 | 5..8 => first, other => local(first) } }";
        let checked = lower_source("l3.allen", source).expect("L3 syntax lowers");
        let outer = checked.module.functions.first().expect("outer function");
        assert!(matches!(outer.return_type, LoweredType::Sequence(_, _)));

        let LoweredStatement::LocalFunction(local) = &outer.body.statements[0] else {
            panic!("expected local function carrier");
        };
        assert_eq!(local.name, "local");
        assert!(matches!(local.return_type, LoweredType::Range(_, _)));
        let local_tail = local.body.tail.as_ref().expect("local function tail");
        let LoweredExprKind::Range {
            inclusive,
            operator_span,
            ..
        } = &local_tail.kind
        else {
            panic!("expected inclusive range carrier");
        };
        assert!(*inclusive);
        assert!(operator_span.start < operator_span.end);

        let LoweredStatement::Let { value: window, .. } = &outer.body.statements[1] else {
            panic!("expected window binding");
        };
        let LoweredExprKind::Slice {
            range,
            bracket_span,
            ..
        } = &window.kind
        else {
            panic!("expected slice carrier");
        };
        assert!(matches!(
            range.kind,
            LoweredExprKind::Range {
                inclusive: false,
                ..
            }
        ));
        assert!(bracket_span.start < bracket_span.end);

        let LoweredStatement::Let { value: first, .. } = &outer.body.statements[2] else {
            panic!("expected first binding");
        };
        assert!(matches!(first.kind, LoweredExprKind::Index { .. }));

        let LoweredExprKind::Match { arms, .. } =
            &outer.body.tail.as_ref().expect("outer match tail").kind
        else {
            panic!("expected match carrier");
        };
        let LoweredPattern::Or {
            alternatives,
            operator_spans,
        } = &arms[0].0
        else {
            panic!("expected OR-pattern carrier");
        };
        assert_eq!(alternatives.len(), 2);
        assert_eq!(operator_spans.len(), 1);
        assert!(matches!(
            alternatives[0],
            LoweredPattern::Range {
                inclusive: true,
                ..
            }
        ));
        assert!(matches!(
            alternatives[1],
            LoweredPattern::Range {
                inclusive: false,
                ..
            }
        ));
    }

    #[test]
    fn multiline_content_cannot_be_less_indented_than_its_closer() {
        let diagnostic =
            checked_compiler_error("fn main() returns String { \"\"\"\n one\n  \"\"\" }");
        assert_eq!(diagnostic.code, "E3005");
    }

    #[test]
    fn builtin_namespace_members_can_use_contextual_or_reserved_spellings() {
        assert_lowers_deterministically(
            "fn f(values: List<Int>) returns Bool { let mapped = list.map(values, fn(item: Int) returns Int { item }); list.any(mapped, fn(item: Int) returns Bool { item > 0 }) }",
        );
    }

    #[test]
    fn forbidden_identifier_roles_have_deterministic_diagnostics() {
        for source in [
            "fn check_case(items: List<Int>) returns Void effects [] { for any in items { } () }",
            "fn check_case(value: Option<Int>) returns Int effects [] { match value { Some(any) => 0, None => 0 } }",
            "fn check_case(value: Result<Int, Int>) returns Int effects [] { match value { Result.Ok(undefined) => 0, Result.Err(_) => 0 } }",
            "record Pair { value: Int } fn check_case(value: Pair) returns Int effects [] { match value { Pair { value: null } => 0 } }",
        ] {
            assert_compiler_error_is_deterministic(source);
        }

        assert_compiler_error_is_deterministic(
            "manifest { language: \"allen-0.1\", entry: main, capabilities: [fs.read(any)] } fn main() returns Void effects [] { () }",
        );
    }

    #[test]
    fn unary_grouped_operand_spans_are_stable() {
        assert_lowers_deterministically(
            "fn spans(flag: Bool, value: Int) returns Int effects [] { let a = !(flag); let b = -(value); let c = await (value); let d = spawn (value); value }",
        );
    }

    #[test]
    fn duplicate_prompt_field_diagnostic_uses_the_repeated_field_name_span() {
        assert_compiler_error_is_deterministic(
            "fn main() returns Int effects [] { prompt { system: \"first\", system: \"second\", output: Int } }",
        );
    }

    #[test]
    fn inline_manifest_matches_the_existing_validated_contract() {
        assert_lowers_deterministically(
            r#"manifest {
  language: "allen-0.1",
  entry: main,
  capabilities: [fs.read(workspace), net.http],
  http_origins: ["https://example.com"],
  tools: { required: [
    { name: "example.echo", version: ">=1.0.0, <2.0.0" },
  ] },
}
fn main() returns Void effects [] { () }
"#,
        );
    }

    #[test]
    fn checked_boundary_matches_representative_committed_examples() {
        for source in [
            include_str!("../../../../examples/data-types.allen"),
            include_str!("../../../../examples/operations.allen"),
            include_str!("../../../../examples/min-int.allen"),
            include_str!("../../../../examples/dynamic-collections.allen"),
        ] {
            assert_lowers_deterministically(source);
        }
    }

    #[test]
    fn refuses_malformed_and_recovered_trees_before_lowering() {
        let source = syntax_source("fn broken(value: Int returns Int { value }");
        let parsed = parse(&source);
        let error = lower_checked(&source, &parsed).expect_err("syntax errors must be refused");
        assert!(matches!(error, SyntaxLoweringError::SyntaxErrors { .. }));
    }

    #[test]
    fn bounded_fallback_is_deterministic_and_refused() {
        let source = syntax_source("fn main() returns Int { [1, 2, 3][0] }");
        let limits = SyntaxLimits {
            nodes: 1,
            diagnostics: 1,
            ..SyntaxLimits::DEFAULT
        };
        let first = parse_with_limits(&source, limits);
        let second = parse_with_limits(&source, limits);
        assert_eq!(first.diagnostics(), second.diagnostics());
        assert_eq!(
            lower_checked(&source, &first).expect_err("first fallback must be refused"),
            lower_checked(&source, &second).expect_err("second fallback must be refused")
        );
        assert!(matches!(
            lower_checked(&source, &first),
            Err(SyntaxLoweringError::SyntaxErrors { .. })
        ));
    }

    #[test]
    fn refuses_a_parse_paired_with_different_source_text() {
        let parsed_source = syntax_source("fn main() returns Void { () }");
        let parsed = parse(&parsed_source);
        let other = syntax_source("fn main() returns Void { { } }");
        assert_eq!(
            lower_checked(&other, &parsed).expect_err("mismatched source must be refused"),
            SyntaxLoweringError::SourceMismatch
        );
    }

    #[test]
    fn refuses_equal_text_parsed_under_a_different_source_identity() {
        let text = "fn main() returns Void { () }";
        let parsed_source = SourceFile::new(SourceFileId::new(0), text).expect("source range");
        let parsed = parse(&parsed_source);
        let other = SourceFile::new(SourceFileId::new(1), text).expect("source range");
        assert_eq!(
            lower_checked(&other, &parsed).expect_err("mismatched source ID must be refused"),
            SyntaxLoweringError::SourceMismatch
        );
    }
}
