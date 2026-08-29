use allen_syntax::{
    AstError, AstNode, CallArgument, Composition, ForStatement, FunctionDeclaration, FunctionType,
    ImportDeclaration, ListLiteral, LocalFunction, MapLiteral, PatternOr, PatternRange, Pipeline,
    Postfix, RecordConstructor, Slice, Source, SourceFile, SourceFileId, SyntaxKind, SyntaxLimits,
    TYPED_AST_ACCESSOR_INVENTORY, TYPED_AST_WRAPPER_INVENTORY, ToolRequirement, parse,
    parse_with_limits,
};
use std::collections::BTreeSet;

const EXPECTED_ROLE_LABELS: &[&str] = &[
    "RecordDeclaration.predicate",
    "ImportDeclaration.extension_keyword",
    "ImportDeclaration.import_source",
    "ManifestField.language_value",
    "ManifestField.entry_name",
    "ManifestField.http_origins",
    "ToolRequirement.tool_name",
    "ToolRequirement.tool_version",
    "ImportName.imported_name",
    "ImportName.local_name",
    "TestDeclaration.test_name",
    "Statement.binding_name",
    "Statement.initializer",
    "Statement.assignment_target",
    "Statement.assignment_value",
    "Statement.return_value",
    "ConditionalExpression.condition",
    "ConditionalExpression.then_branch",
    "ConditionalExpression.else_if",
    "ConditionalExpression.else_branch",
    "ForStatement.iterable",
    "NamedType.segments",
    "GenericType.type_argument",
    "GenericType.first_type_argument",
    "GenericType.second_type_argument",
    "FunctionType.parameter_types",
    "FunctionType.return_type",
    "Parameter.default_value",
    "Postfix.field_names",
    "Postfix.optional_field_names",
    "Postfix.call_type_arguments",
    "Slice.index",
    "CallArgument.argument_label",
    "CallArgument.value",
    "TemplateLiteral.open_backtick",
    "TemplateLiteral.close_backtick",
    "TemplateLiteral.open_multiline_delimiter",
    "TemplateLiteral.close_multiline_delimiter",
    "EnumRecordConstructor.enum_name",
    "EnumRecordConstructor.variant_name",
    "QualifiedEnum.enum_name",
    "QualifiedEnum.variant_name",
    "RecordUpdateBase.base",
    "ListItem.spread",
    "ListItem.value",
    "MapItem.spread",
    "MapItem.key",
    "MapItem.value",
    "MatchExpression.scrutinee",
    "PatternPrimary.binding_name",
    "EnumPattern.enum_name",
    "EnumPattern.variant_name",
    "PatternField.field_name",
    "PromptField.system_value",
    "PromptField.context_value",
    "PromptField.data_value",
    "PromptField.output_type",
    "PromptField.max_attempts_value",
    "ShortClosure.parameter_names",
    "ShortClosure.body",
];

fn source_file(id: u32, text: &str) -> SourceFile {
    SourceFile::new(SourceFileId::new(id), text).expect("small typed AST fixture")
}

fn typed_source(id: u32, text: &str) -> Source {
    let source = source_file(id, text);
    let parsed = parse(&source);
    assert!(
        parsed.diagnostics().is_empty(),
        "unexpected syntax diagnostics: {:?}",
        parsed.diagnostics()
    );
    Source::cast(parsed.syntax()).expect("source typed wrapper")
}

fn functions(source: &Source) -> impl Iterator<Item = FunctionDeclaration> + '_ {
    source
        .declarations()
        .filter_map(|declaration| declaration.function_declaration())
}

#[test]
fn top_level_constant_accessors_preserve_the_declared_contract() {
    let source = typed_source(
        41,
        "export const SharedAnswer: Int = 40 + 2; export fn main() returns Int { SharedAnswer }",
    );
    let constant = source
        .declarations()
        .find_map(|declaration| declaration.const_declaration())
        .expect("constant declaration");
    assert_eq!(constant.export_token().unwrap().text(), "export");
    assert_eq!(constant.const_token().unwrap().text(), "const");
    assert_eq!(constant.ident_token().unwrap().text(), "SharedAnswer");
    assert_eq!(
        constant.ty().unwrap().syntax().text().to_string().trim(),
        "Int"
    );
    assert_eq!(
        constant
            .expression()
            .unwrap()
            .syntax()
            .text()
            .to_string()
            .trim(),
        "40 + 2"
    );
    assert_eq!(constant.semi_token().unwrap().text(), ";");
}

#[test]
fn top_level_constants_require_a_type_and_terminator() {
    for text in [
        "const MissingType = 1; export fn main() returns Int { 0 }",
        "const MissingTerminator: Int = 1 export fn main() returns Int { 0 }",
    ] {
        let source = source_file(42, text);
        let parsed = parse(&source);
        assert!(
            parsed.has_errors(),
            "invalid constant syntax must be rejected"
        );
        assert!(
            parsed
                .syntax()
                .descendants()
                .any(|node| node.kind() == SyntaxKind::FunctionDeclaration),
            "recovery must retain the following function"
        );
    }
}

#[test]
fn typed_accessors_expose_valid_children_and_tokens_without_losing_text() {
    let text = r#"manifest {
  language: "0.1",
  entry: main,
  capabilities: [fs.read(workdir),],
  tools: { required: [{ name: "demo", version: "1", },], },
}
import { Box as LocalBox, } from "./support.allen";
export type Values = List<Int>
export async fn main<T: Eq,>(value: List<T>,) returns Result<T, String> effects [fs.read,] {
  return Ok(value[0]);
}
"#;
    let source = typed_source(1, text);

    let manifest = source.inline_manifest().expect("inline manifest");
    assert_eq!(manifest.manifest_token().unwrap().text(), "manifest");
    assert_eq!(manifest.manifest_fields().count(), 4);
    assert_eq!(manifest.comma_tokens().count(), 4);

    let import = source.import_declarations().next().expect("import");
    let import_name = import.import_names().next().expect("import name");
    assert_eq!(
        [
            import_name.imported_name_token().unwrap().text(),
            import_name.local_name_token().unwrap().text(),
        ],
        ["Box", "LocalBox"]
    );
    assert_eq!(
        import.import_source_token().unwrap().text(),
        "\"./support.allen\""
    );

    let alias = source
        .declarations()
        .find_map(|declaration| declaration.type_alias_declaration())
        .expect("type alias");
    assert_eq!(alias.export_token().unwrap().text(), "export");
    assert_eq!(alias.type_token().unwrap().text(), "type");
    assert_eq!(alias.ident_token().unwrap().text(), "Values");
    assert_eq!(alias.eq_token().unwrap().text(), "=");
    assert!(alias.ty().is_some());
    assert!(alias.errors().next().is_none());

    let function = functions(&source).next().expect("function");
    assert_eq!(function.export_token().unwrap().text(), "export");
    assert_eq!(function.async_token().unwrap().text(), "async");
    assert_eq!(function.ident_token().unwrap().text(), "main");
    assert_eq!(function.parameters().count(), 1);
    assert!(function.generic_parameters().is_some());
    assert!(function.ty().is_some());
    assert!(function.effect_clause().is_some());
    assert!(function.body().is_some());
    assert!(function.errors().next().is_none());
    assert_eq!(source.eof_token().unwrap().text(), "");

    assert_eq!(source.syntax().text().to_string(), text);
    let round_trip = source
        .syntax()
        .descendants_with_tokens()
        .filter_map(rowan::NodeOrToken::into_token)
        .filter(|token| token.kind() != SyntaxKind::Eof)
        .map(|token| token.text().to_owned())
        .collect::<String>();
    assert_eq!(round_trip, text);
}

#[test]
fn generated_inventory_pins_the_complete_public_coverage_contract() {
    assert_eq!(TYPED_AST_WRAPPER_INVENTORY.len(), 83);
    assert_eq!(TYPED_AST_ACCESSOR_INVENTORY.len(), 474);
    assert_eq!(TYPED_AST_WRAPPER_INVENTORY, allen_syntax::NODE_INVENTORY);

    let wrappers = TYPED_AST_WRAPPER_INVENTORY
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    assert_eq!(wrappers.len(), TYPED_AST_WRAPPER_INVENTORY.len());

    let mut methods = BTreeSet::new();
    for &(wrapper, method, target, cardinality, label) in TYPED_AST_ACCESSOR_INVENTORY {
        assert!(wrappers.contains(wrapper));
        assert!(methods.insert((wrapper, method)));
        assert!(target.starts_with("node:") || target.starts_with("token:"));
        assert!(matches!(cardinality, "required" | "optional" | "many"));
        assert!(label.is_empty() || method.starts_with(label));
    }
    assert!(wrappers.iter().all(|wrapper| {
        TYPED_AST_ACCESSOR_INVENTORY
            .iter()
            .any(|(owner, _, _, _, _)| owner == wrapper)
    }));

    let actual_labels = TYPED_AST_ACCESSOR_INVENTORY
        .iter()
        .filter(|(_, _, _, _, label)| !label.is_empty())
        .map(|(wrapper, _, _, _, label)| format!("{wrapper}.{label}"))
        .collect::<BTreeSet<_>>();
    let expected_labels = EXPECTED_ROLE_LABELS
        .iter()
        .map(|label| (*label).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_labels, expected_labels);
}

fn node_text<N>(node: &N) -> String
where
    N: AstNode<Language = allen_syntax::AllenLanguage>,
{
    node.syntax().text().to_string().trim().to_owned()
}

#[test]
fn labeled_function_type_roles_do_not_shift_after_a_missing_parameter() {
    let valid = typed_source(
        4,
        "fn main(callback: fn(Int, String) returns Bool) returns Void { () }\n",
    );
    let function_type = valid
        .syntax()
        .descendants()
        .find_map(FunctionType::cast)
        .expect("function type");
    assert_eq!(
        function_type
            .parameter_types()
            .map(|ty| node_text(&ty))
            .collect::<Vec<_>>(),
        ["Int", "String"]
    );
    assert_eq!(node_text(&function_type.return_type().unwrap()), "Bool");

    let source = source_file(
        5,
        "fn main(callback: fn(, String) returns Bool) returns Void { () }\n",
    );
    let parsed = parse(&source);
    assert!(parsed.has_errors());
    parsed.assert_round_trip(&source);
    let function_type = parsed
        .syntax()
        .descendants()
        .find_map(FunctionType::cast)
        .expect("recovered function type");
    assert_eq!(
        function_type
            .parameter_types()
            .map(|ty| node_text(&ty))
            .collect::<Vec<_>>(),
        ["", "String"]
    );
    assert_eq!(node_text(&function_type.return_type().unwrap()), "Bool");
}

#[test]
fn labeled_function_type_accessors_scale_across_large_valid_parameter_lists() {
    for (id, count) in [100, 200, 400, 800, 1_600].into_iter().enumerate() {
        let parameters = vec!["Int"; count].join(", ");
        let text =
            format!("fn main(callback: fn({parameters}) returns Bool) returns Void {{ () }}\n");
        let source = typed_source(u32::try_from(id + 20).unwrap(), &text);
        let function_type = source
            .syntax()
            .descendants()
            .find_map(FunctionType::cast)
            .expect("function type");

        assert_eq!(function_type.parameter_types().count(), count);
        assert_eq!(node_text(&function_type.return_type().unwrap()), "Bool");
    }
}

#[test]
fn for_source_uses_the_expression_range_carrier() {
    let valid = typed_source(
        6,
        "fn main() returns Void { for item in start..end { } () }\n",
    );
    let for_statement = valid
        .syntax()
        .descendants()
        .find_map(ForStatement::cast)
        .expect("for statement");
    assert_eq!(node_text(&for_statement.iterable().unwrap()), "start..end");
    let range = for_statement
        .iterable()
        .unwrap()
        .range()
        .expect("for expression has a range layer");
    assert_eq!(range.coalescings().count(), 2);
    assert!(range.dot_dot_token().is_some());

    let source = source_file(7, "fn main() returns Void { for item in ..end { } () }\n");
    let parsed = parse(&source);
    assert!(parsed.has_errors());
    parsed.assert_round_trip(&source);
    let for_statement = parsed
        .syntax()
        .descendants()
        .find_map(ForStatement::cast)
        .expect("recovered for statement");
    assert_eq!(node_text(&for_statement.iterable().unwrap()), "..end");
}

#[test]
fn l3_typed_roles_cover_ranges_slices_patterns_and_local_functions() {
    let source = typed_source(
        61,
        "fn outer(values: List<Int>, subject: T) returns Sequence<Int> { fn local(limit: Int) returns Range<Int> { 0..=limit } let window = values[1..3]; let first = values[0]; match subject { 1..=3 | 5..8 => first, other => local(first) } }\n",
    );

    let local = source
        .syntax()
        .descendants()
        .find_map(LocalFunction::cast)
        .expect("local function");
    assert_eq!(local.ident_token().unwrap().text(), "local");
    assert_eq!(local.parameters().count(), 1);
    assert_eq!(node_text(&local.ty().unwrap()), "Range<Int>");
    assert!(local.body().is_some());

    let brackets = source
        .syntax()
        .descendants()
        .filter_map(Slice::cast)
        .collect::<Vec<_>>();
    assert_eq!(brackets.len(), 2);
    assert_eq!(node_text(&brackets[0].index().unwrap()), "1..3");
    assert_eq!(node_text(&brackets[1].index().unwrap()), "0");
    assert!(
        brackets.iter().all(|slice| {
            slice.l_bracket_token().is_some() && slice.r_bracket_token().is_some()
        })
    );

    let or_pattern = source
        .syntax()
        .descendants()
        .filter_map(PatternOr::cast)
        .find(|pattern| pattern.pipe_tokens().next().is_some())
        .expect("OR pattern");
    assert_eq!(or_pattern.pattern_primaries().count(), 2);
    assert_eq!(or_pattern.pipe_tokens().count(), 1);
    let ranges = or_pattern
        .syntax()
        .descendants()
        .filter_map(PatternRange::cast)
        .collect::<Vec<_>>();
    assert_eq!(ranges.len(), 2);
    assert!(ranges[0].dot_dot_eq_token().is_some());
    assert!(ranges[1].dot_dot_token().is_some());
    assert!(ranges.iter().all(|range| range.literals().count() == 2));
}

#[test]
fn labeled_map_roles_partition_entries_and_recovered_values() {
    let valid = typed_source(
        8,
        "fn main() returns Void { let values = map { first: 1, second: 2 }; () }\n",
    );
    let map = valid
        .syntax()
        .descendants()
        .find_map(MapLiteral::cast)
        .expect("map literal");
    assert_eq!(
        map.map_items()
            .map(|item| node_text(&item.key().unwrap()))
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(
        map.map_items()
            .map(|item| node_text(&item.value().unwrap()))
            .collect::<Vec<_>>(),
        ["1", "2"]
    );

    let source = source_file(
        9,
        "fn main() returns Void { let values = map { first: , second: 2 }; () }\n",
    );
    let parsed = parse(&source);
    assert!(parsed.has_errors());
    parsed.assert_round_trip(&source);
    let map = parsed
        .syntax()
        .descendants()
        .find_map(MapLiteral::cast)
        .expect("recovered map literal");
    assert_eq!(
        map.map_items()
            .map(|item| node_text(&item.key().unwrap()))
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(
        map.map_items()
            .map(|item| node_text(&item.value().unwrap()))
            .collect::<Vec<_>>(),
        ["", "2"]
    );
}

#[test]
fn labeled_tool_tokens_do_not_shift_when_the_name_value_is_missing() {
    let valid = typed_source(
        10,
        "manifest { language: \"0.1\", entry: main, tools: { required: [{ name: \"demo\", version: \"2\" }] } } fn main() returns Void { () }\n",
    );
    let tool = valid
        .syntax()
        .descendants()
        .find_map(ToolRequirement::cast)
        .expect("tool requirement");
    assert_eq!(tool.tool_name_token().unwrap().text(), "\"demo\"");
    assert_eq!(tool.tool_version_token().unwrap().text(), "\"2\"");

    let source = source_file(
        11,
        "manifest { language: \"0.1\", entry: main, tools: { required: [{ name: , version: \"2\" }] } } fn main() returns Void { () }\n",
    );
    let parsed = parse(&source);
    assert!(parsed.has_errors());
    parsed.assert_round_trip(&source);
    let tool = parsed
        .syntax()
        .descendants()
        .find_map(ToolRequirement::cast)
        .expect("recovered tool requirement");
    assert!(tool.tool_name_token().is_none());
    assert_eq!(tool.tool_version_token().unwrap().text(), "\"2\"");
}

#[test]
fn malformed_and_bounded_fallback_trees_remain_explicit_typed_views() {
    let text = "fn broken() returns Int\nfn later() returns Int { 2 }\n";
    let malformed_source = source_file(2, text);
    let parsed = parse(&malformed_source);
    parsed.assert_round_trip(&malformed_source);
    assert!(parsed.has_errors());
    let source = Source::cast(parsed.syntax()).expect("recovered source wrapper");
    let functions = functions(&source).collect::<Vec<_>>();
    assert_eq!(
        functions.len(),
        2,
        "recovery must retain the later declaration"
    );

    let broken = &functions[0];
    assert!(broken.ty().is_some());
    assert!(broken.body().is_none());
    assert_eq!(
        broken
            .errors()
            .map(|error| error.kind())
            .collect::<Vec<_>>(),
        [SyntaxKind::Missing]
    );
    assert!(functions[1].body().is_some());
    assert!(functions[1].errors().next().is_none());
    assert_eq!(source.syntax().text().to_string(), text);

    let fallback_text = "fn fallback() returns Int { 1 }";
    let fallback_source = source_file(3, fallback_text);
    let fallback = parse_with_limits(
        &fallback_source,
        SyntaxLimits {
            nodes: 1,
            ..SyntaxLimits::DEFAULT
        },
    );
    fallback.assert_round_trip(&fallback_source);
    let fallback = Source::cast(fallback.syntax()).expect("fallback source wrapper");
    assert!(matches!(
        fallback.errors().next(),
        Some(AstError::ErrorToken(token)) if token.text() == fallback_text
    ));
    assert!(fallback.declarations().next().is_none());
}

#[test]
fn source_tests_have_typed_accessors_and_recover_to_later_declarations() {
    let source = typed_source(
        50,
        "test \"pure\" { () } test \"effectful\" effects [agent.message] { () }",
    );
    let tests = source
        .declarations()
        .filter_map(|declaration| declaration.test_declaration())
        .collect::<Vec<_>>();
    assert_eq!(tests.len(), 2);
    assert_eq!(tests[0].test_name_token().unwrap().text(), "\"pure\"");
    assert!(tests[0].effect_clause().is_none());
    assert!(tests[1].effect_clause().is_some());

    let text = "test missing-name { () }\nfn later() returns Void { () }";
    let file = source_file(51, text);
    let parsed = parse(&file);
    parsed.assert_round_trip(&file);
    assert!(parsed.has_errors());
    let source = Source::cast(parsed.syntax()).unwrap();
    assert!(
        source
            .declarations()
            .any(|declaration| declaration.test_declaration().is_some())
    );
    assert!(
        source
            .declarations()
            .any(|declaration| declaration.function_declaration().is_some())
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn l2_surface_forms_have_lossless_typed_roles_and_fixed_precedence() {
    let text = r#"import extension { render as show } from "ui";
record User { active: Bool }
fn configured(count: Int = 3, delay: Int = count) returns Int { count + delay }
fn main() returns Void {
  let updated = User { ..user, active: true };
  let values = [..prefix, item, ..suffix];
  let settings = map { ..defaults, "mode": selected };
  let warn = log.write(level: "warn", message: _);
  let folded = list.fold(values, 0) fn(total, value) => total + value;
  let chained = maybe?.user?.show();
  let result = input |> normalize >> render ?? fallback;
  ()
}
"#;
    let source = typed_source(60, text);

    let import = source
        .syntax()
        .descendants()
        .find_map(ImportDeclaration::cast)
        .expect("extension import");
    assert_eq!(
        import.extension_keyword_token().unwrap().text(),
        "extension"
    );

    let configured = functions(&source)
        .find(|function| function.ident_token().unwrap().text() == "configured")
        .expect("configured function");
    let defaults = configured
        .parameters()
        .map(|parameter| parameter.default_value().map(|value| node_text(&value)))
        .collect::<Vec<_>>();
    assert_eq!(defaults, [Some("3".to_owned()), Some("count".to_owned())]);

    let record = source
        .syntax()
        .descendants()
        .find_map(RecordConstructor::cast)
        .expect("record update");
    assert_eq!(
        node_text(&record.record_update_base().unwrap().base().unwrap()),
        "user"
    );

    let list = source
        .syntax()
        .descendants()
        .find_map(ListLiteral::cast)
        .expect("list spread");
    assert_eq!(
        list.list_items()
            .map(|item| item.dot_dot_token().is_some())
            .collect::<Vec<_>>(),
        [true, false, true]
    );

    let map = source
        .syntax()
        .descendants()
        .find_map(MapLiteral::cast)
        .expect("map spread");
    let map_items = map.map_items().collect::<Vec<_>>();
    assert!(map_items[0].spread().is_some());
    assert_eq!(node_text(&map_items[1].key().unwrap()), "\"mode\"");
    assert_eq!(node_text(&map_items[1].value().unwrap()), "selected");

    let hole = source
        .syntax()
        .descendants()
        .filter_map(CallArgument::cast)
        .find(|argument| argument.underscore_token().is_some())
        .expect("call placeholder");
    assert_eq!(hole.argument_label_token().unwrap().text(), "message");
    assert!(hole.value().is_none());

    let trailing = source
        .syntax()
        .descendants()
        .filter_map(Postfix::cast)
        .find(|postfix| postfix.short_closures().next().is_some())
        .expect("trailing callback");
    assert_eq!(trailing.short_closures().count(), 1);

    let optional = source
        .syntax()
        .descendants()
        .filter_map(Postfix::cast)
        .find(|postfix| postfix.question_dot_tokens().count() == 2)
        .expect("optional chain");
    assert_eq!(
        optional
            .optional_field_names_tokens()
            .map(|token| token.text().to_owned())
            .collect::<Vec<_>>(),
        ["user", "show"]
    );

    let pipeline = source
        .syntax()
        .descendants()
        .filter_map(Pipeline::cast)
        .find(|pipeline| pipeline.pipe_gt_tokens().next().is_some())
        .expect("pipeline");
    assert_eq!(pipeline.compositions().count(), 2);
    let composed = pipeline
        .compositions()
        .nth(1)
        .expect("composed pipeline stage");
    assert_eq!(composed.disjunctions().count(), 2);
    assert_eq!(composed.gt_tokens().count(), 2);
    assert_eq!(
        node_text(
            &source
                .syntax()
                .descendants()
                .filter_map(Composition::cast)
                .find(|composition| composition.gt_tokens().next().is_some())
                .unwrap()
        ),
        "normalize >> render"
    );
}

#[test]
fn l2_recovery_keeps_later_declarations_after_malformed_spreads_and_callbacks() {
    let text = "fn broken() returns Void { let values = [.., item]; let call = f() fn(x) => x fn(y) => y; () }\nfn later() returns Int { 1 }\n";
    let file = source_file(61, text);
    let parsed = parse(&file);
    parsed.assert_round_trip(&file);
    assert!(parsed.has_errors());
    let source = Source::cast(parsed.syntax()).expect("recovered source");
    assert!(functions(&source).any(|function| function.ident_token().unwrap().text() == "later"));
}
