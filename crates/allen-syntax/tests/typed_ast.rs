use allen_syntax::{
    AstError, AstNode, ForStatement, FunctionDeclaration, FunctionType, MapLiteral, Source,
    SourceFile, SourceFileId, SyntaxKind, SyntaxLimits, TYPED_AST_ACCESSOR_INVENTORY,
    TYPED_AST_WRAPPER_INVENTORY, ToolRequirement, parse, parse_with_limits,
};
use std::collections::BTreeSet;

const EXPECTED_ROLE_LABELS: &[&str] = &[
    "ImportDeclaration.import_source",
    "ManifestField.language_value",
    "ManifestField.entry_name",
    "ManifestField.http_origins",
    "ToolRequirement.tool_name",
    "ToolRequirement.tool_version",
    "ImportName.imported_name",
    "ImportName.local_name",
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
    "ForStatement.range_end",
    "NamedType.segments",
    "GenericType.type_argument",
    "GenericType.first_type_argument",
    "GenericType.second_type_argument",
    "FunctionType.parameter_types",
    "FunctionType.return_type",
    "Postfix.indices",
    "Postfix.field_names",
    "Postfix.call_type_arguments",
    "Postfix.arguments",
    "TemplateLiteral.open_backtick",
    "TemplateLiteral.close_backtick",
    "EnumRecordConstructor.enum_name",
    "EnumRecordConstructor.variant_name",
    "QualifiedEnum.enum_name",
    "QualifiedEnum.variant_name",
    "MapLiteral.keys",
    "MapLiteral.values",
    "MatchExpression.scrutinee",
    "Pattern.binding_name",
    "EnumPattern.enum_name",
    "EnumPattern.variant_name",
    "EnumPattern.binding_names",
    "PatternField.field_name",
    "PatternField.binding_name",
    "PromptField.system_value",
    "PromptField.context_value",
    "PromptField.data_value",
    "PromptField.output_type",
    "PromptField.max_attempts_value",
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
    assert_eq!(TYPED_AST_WRAPPER_INVENTORY.len(), 66);
    assert_eq!(TYPED_AST_ACCESSOR_INVENTORY.len(), 392);
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
fn labeled_for_roles_keep_range_end_distinct_from_a_missing_iterable() {
    let valid = typed_source(
        6,
        "fn main() returns Void { for item in start..end { } () }\n",
    );
    let for_statement = valid
        .syntax()
        .descendants()
        .find_map(ForStatement::cast)
        .expect("for statement");
    assert_eq!(node_text(&for_statement.iterable().unwrap()), "start");
    assert_eq!(node_text(&for_statement.range_end().unwrap()), "end");

    let source = source_file(7, "fn main() returns Void { for item in ..end { } () }\n");
    let parsed = parse(&source);
    assert!(parsed.has_errors());
    parsed.assert_round_trip(&source);
    let for_statement = parsed
        .syntax()
        .descendants()
        .find_map(ForStatement::cast)
        .expect("recovered for statement");
    assert_eq!(node_text(&for_statement.iterable().unwrap()), "");
    assert_eq!(node_text(&for_statement.range_end().unwrap()), "end");
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
        map.keys().map(|key| node_text(&key)).collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(
        map.values()
            .map(|value| node_text(&value))
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
        map.keys().map(|key| node_text(&key)).collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(
        map.values()
            .map(|value| node_text(&value))
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
