use allen_bytecode::{MAX_VALUE_NESTING, verify};
use allen_compiler::{Diagnostic, Span, compile_bundle, compile_source};
use allen_vm::Value;
use std::collections::BTreeMap;

#[test]
fn transparent_aliases_cover_scalars_collections_structures_and_nominal_types() {
    let source = r#"
type Count = Int
type Counts = List<Count>
record Point { x: Int, y: Int }
type Coordinate = Point
type LabeledPoint = { label: String, point: Coordinate }

export fn main() returns Int {
    let values: Counts = [40, 2];
    let point: Coordinate = Coordinate { x: values[0], y: values[1] };
    let labeled: LabeledPoint = { label: "answer", point };
    labeled.point.x + labeled.point.y
}
"#;
    let compilation = compile_source(source).expect("transparent aliases compile");
    let verified = verify(compilation.module).expect("alias bytecode verifies");
    assert_eq!(
        allen_vm::execute(&verified).expect("alias example executes"),
        Value::Int(42)
    );
}

#[test]
fn aliases_support_forward_chains_enums_and_function_types() {
    let source = r"
type First = Second
type Second = Choice
enum Choice { Number(Int) }
type Callback = fn(Int) returns Int effects []
record Handler { callback: Callback }

export fn main() returns Int {
    let selected: First = Choice.Number(42);
    let handler: Handler = Handler {
        callback: fn(value: Int) returns Int { value },
    };
    let retained = handler;
    match selected { Choice.Number(value) => value }
}
";
    compile_source(source).expect("forward, nominal, and callback aliases compile");
}

#[test]
fn unknown_and_duplicate_aliases_keep_stable_type_diagnostics() {
    let unknown = "type Missing = DoesNotExist\nexport fn main() returns Int { 0 }\n";
    let diagnostics = compile_source(unknown).expect_err("unknown alias target is rejected");
    assert_eq!(
        diagnostics,
        [Diagnostic {
            code: "E3005",
            message: "unknown type 'DoesNotExist'".to_owned(),
            span: Span { start: 15, end: 27 },
            labels: Vec::new(),
            source: Some("main.allen".to_owned()),
        }]
    );

    let duplicate = "type Name = String\ntype Name = Int\nexport fn main() returns Int { 0 }\n";
    let diagnostics = compile_source(duplicate).expect_err("type namespace collision is rejected");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "E3005");
    assert_eq!(diagnostics[0].message, "duplicate type 'Name'");
    assert_eq!(diagnostics[0].span, Span { start: 24, end: 28 });
}

#[test]
fn alias_cycles_are_bounded_and_deterministic() {
    let source = concat!(
        "type A = List<B>\n",
        "type B = Option<A>\n",
        "export fn main() returns Int { 0 }\n",
    );
    let first = compile_source(source).expect_err("alias cycle is rejected");
    let second = compile_source(source).expect_err("alias cycle is rejected deterministically");
    assert_eq!(first, second);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].code, "E3005");
    assert_eq!(first[0].message, "cyclic type alias involving 'A'");
    assert_eq!(first[0].span, Span { start: 5, end: 6 });
    assert_eq!(first[0].source.as_deref(), Some("main.allen"));
}

#[test]
fn exported_aliases_import_like_other_named_types() {
    let sources = BTreeMap::from([
        (
            "main.allen".to_owned(),
            concat!(
                "import { PublicCount as LocalCount } from \"./support.allen\";\n",
                "export fn main() returns LocalCount { 42 }\n",
            )
            .to_owned(),
        ),
        (
            "support.allen".to_owned(),
            "export type PublicCount = Int\n".to_owned(),
        ),
    ]);
    compile_bundle("main.allen", &sources).expect("an exported alias is importable");

    let private_sources = BTreeMap::from([
        (
            "main.allen".to_owned(),
            concat!(
                "import { PrivateCount } from \"./support.allen\";\n",
                "export fn main() returns PrivateCount { 42 }\n",
            )
            .to_owned(),
        ),
        (
            "support.allen".to_owned(),
            "type PrivateCount = Int\n".to_owned(),
        ),
    ]);
    let diagnostics = compile_bundle("main.allen", &private_sources)
        .expect_err("a private alias is not importable");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "E3003");
    assert_eq!(
        diagnostics[0].message,
        "type 'PrivateCount' is private to module 'support.allen'"
    );
}

#[test]
fn aggregate_alias_expansion_stops_at_the_value_nesting_limit() {
    let source_with_depth = |alias_count| {
        let mut source = String::new();
        for index in 0..alias_count {
            source.push_str(&format!("type Alias{index} = List<Alias{}>\n", index + 1));
        }
        source.push_str(&format!("type Alias{alias_count} = Int\n"));
        source.push_str("export fn main() returns Int { 0 }\n");
        source
    };

    compile_source(&source_with_depth(MAX_VALUE_NESTING))
        .expect("an alias shape at the value nesting limit compiles");

    let source = source_with_depth(MAX_VALUE_NESTING * 32);
    let first = compile_source(&source).expect_err("over-deep alias expansion is rejected");
    let second = compile_source(&source).expect_err("alias depth rejection is deterministic");
    assert_eq!(first, second);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].code, "E2012");
    assert_eq!(first[0].message, "type nesting exceeds the language limit");
    assert_eq!(first[0].source.as_deref(), Some("main.allen"));
}

#[test]
fn alias_depth_includes_expanded_named_record_targets() {
    let source_with_records = |record_count: usize| {
        let mut source = String::new();
        for index in 0..record_count {
            if index + 1 == record_count {
                source.push_str(&format!("record R{index:03} {{ value: Int }}\n"));
            } else {
                source.push_str(&format!(
                    "record R{index:03} {{ next: R{:03} }}\n",
                    index + 1
                ));
            }
        }
        source.push_str("type Wrapped = List<R000>\n");
        source.push_str("export fn main() returns Int { 0 }\n");
        source
    };

    compile_source(&source_with_records(MAX_VALUE_NESTING - 1))
        .expect("127 records plus List has the allowed expanded depth of 128");

    let over_limit = source_with_records(MAX_VALUE_NESTING);
    let first = compile_source(&over_limit)
        .expect_err("128 records plus List has expanded depth 129 and is rejected");
    let second = compile_source(&over_limit).expect_err("record alias depth is deterministic");
    let start = over_limit.find("Wrapped").expect("alias name is present");
    assert_eq!(first, second);
    assert_eq!(
        first,
        [Diagnostic {
            code: "E2012",
            message: "type alias nesting exceeds the language limit".to_owned(),
            span: Span {
                start,
                end: start + "Wrapped".len(),
            },
            labels: Vec::new(),
            source: Some("main.allen".to_owned()),
        }]
    );
}

#[test]
fn aliased_record_fields_do_not_depend_on_record_name_order() {
    for (container, target) in [("Alpha", "Zebra"), ("Zebra", "Alpha")] {
        let source = format!(
            r"
type Target = {target}
record {container} {{ nested: Target }}
record {target} {{ value: Int }}

export fn main() returns Int {{
    let target: Target = {target} {{ value: 42 }};
    let container: {container} = {container} {{ nested: target }};
    container.nested.value
}}
"
        );
        let compilation = compile_source(&source).unwrap_or_else(|diagnostics| {
            panic!("{container} -> {target} failed: {diagnostics:?}")
        });
        let verified = verify(compilation.module).expect("record-order bytecode verifies");
        assert_eq!(
            allen_vm::execute(&verified).expect("record-order example executes"),
            Value::Int(42)
        );
    }
}

#[test]
fn imported_enum_alias_constructs_and_matches_a_private_nominal_target() {
    let sources = BTreeMap::from([
        (
            "main.allen".to_owned(),
            concat!(
                "import { Public } from \"./support.allen\";\n",
                "export fn main() returns Int {\n",
                "    let selected: Public = Public.Value(42);\n",
                "    match selected { Public.Value(number) => number }\n",
                "}\n",
            )
            .to_owned(),
        ),
        (
            "support.allen".to_owned(),
            concat!(
                "enum Hidden { Value(Int) }\n",
                "export type Public = Hidden\n",
            )
            .to_owned(),
        ),
    ]);
    let compilation = compile_bundle("main.allen", &sources)
        .expect("an imported enum alias can expose a private nominal target transparently");
    let verified = verify(compilation.module).expect("imported enum-alias bytecode verifies");
    assert_eq!(
        allen_vm::execute(&verified).expect("imported enum-alias example executes"),
        Value::Int(42)
    );
}

#[test]
fn aliases_cannot_shadow_non_shadowable_builtin_types() {
    let source = "type Int = String\nexport fn main() returns Int { 0 }\n";
    let diagnostics = compile_source(source).expect_err("builtin alias collision is rejected");
    assert_eq!(
        diagnostics,
        [Diagnostic {
            code: "E3005",
            message: "type alias 'Int' conflicts with built-in type 'Int'".to_owned(),
            span: Span { start: 5, end: 8 },
            labels: Vec::new(),
            source: Some("main.allen".to_owned()),
        }]
    );
}
