use allen_bytecode::{RecordField, ValueType, encode, verify};
use allen_compiler::{
    CompilerToolBinding, PackageEntryPoint, PackageSourceBundle, assemble_loose_compilation,
    compile_package_bundle_with_tools, compile_source,
};
use allen_vm::{EnumIdentity, EnumPayload, Value, execute};
use std::collections::BTreeMap;

fn execute_source(source: &str) -> Value {
    let compilation = compile_source(source).expect("combinator source compiles");
    execute(&verify(compilation.module).expect("combinator bytecode verifies"))
        .expect("combinator source executes")
}

fn some(value: Value) -> Value {
    Value::Enum(std::rc::Rc::new(allen_vm::EnumValue {
        identity: EnumIdentity::Option,
        type_name: "Option".into(),
        variant: 1,
        variant_name: "Some".into(),
        payload: EnumPayload::Tuple(vec![value].into()),
    }))
}

fn some_none() -> Value {
    Value::Enum(std::rc::Rc::new(allen_vm::EnumValue {
        identity: EnumIdentity::Option,
        type_name: "Option".into(),
        variant: 0,
        variant_name: "None".into(),
        payload: EnumPayload::Unit,
    }))
}

#[test]
fn eager_combinators_preserve_order_and_generic_result_shapes() {
    let value = execute_source(
        r"
export fn main() returns (List<Int>, List<Int>, List<Int>, List<Int>, Option<Int>, Bool, Bool, Int, Int, List<Int>) {
  let values = [1, 2, 3];
  let mapped = list.map(values, fn(item: Int) returns Int { item * 10 });
  let filtered = list.filter(values, fn(item: Int) returns Bool { item % 2 == 1 });
  let flattened = list.flat_map(values, fn(item: Int) returns List<Int> { [item, item * 10] });
  let filtered_mapped = list.filter_map(values, fn(item: Int) returns Option<Int> {
    let absent: Option<Int> = None;
    if (item % 2 == 0) { Some(item * 10) } else { absent }
  });
  let found = list.find(values, fn(item: Int) returns Bool { item == 2 });
  let has_any = list.any(values, fn(item: Int) returns Bool { item == 2 });
  let all = list.all(values, fn(item: Int) returns Bool { item < 4 });
  let partitioned = list.partition(values, fn(item: Int) returns Bool { item % 2 == 1 });
  let scanned = list.scan(values, 0, fn(total: Int, item: Int) returns Int { total + item });
  (mapped, filtered, flattened, filtered_mapped, found, has_any, all,
   length(partitioned.matched), length(partitioned.rest), scanned)
}
",
    );
    assert_eq!(
        value,
        Value::Tuple(
            vec![
                Value::List(vec![Value::Int(10), Value::Int(20), Value::Int(30)].into()),
                Value::List(vec![Value::Int(1), Value::Int(3)].into()),
                Value::List(
                    vec![
                        Value::Int(1),
                        Value::Int(10),
                        Value::Int(2),
                        Value::Int(20),
                        Value::Int(3),
                        Value::Int(30)
                    ]
                    .into()
                ),
                Value::List(vec![Value::Int(20)].into()),
                some(Value::Int(2)),
                Value::Bool(true),
                Value::Bool(true),
                Value::Int(2),
                Value::Int(1),
                Value::List(vec![Value::Int(1), Value::Int(3), Value::Int(6)].into()),
            ]
            .into(),
        )
    );
}

#[test]
fn predicates_short_circuit_before_a_callback_trap() {
    let value = execute_source(
        r#"
export fn main() returns (Option<Int>, Bool, Bool) {
  let values = [1, 2, 3];
  let found = list.find(values, fn(item: Int) returns Bool {
    if (item == 3) { fail("find must short-circuit") } else { item == 2 }
  });
  let has_any = list.any(values, fn(item: Int) returns Bool {
    if (item == 3) { fail("any must short-circuit") } else { item == 2 }
  });
  let all = list.all(values, fn(item: Int) returns Bool {
    if (item == 3) { fail("all must short-circuit") } else { item < 2 }
  });
  (found, has_any, all)
}
"#,
    );
    assert_eq!(
        value,
        Value::Tuple(vec![some(Value::Int(2)), Value::Bool(true), Value::Bool(false)].into())
    );
}

#[test]
fn combinators_reject_effectful_and_mismatched_callbacks() {
    for source in [
        r"export fn main() returns List<Int> { list.map([1], fn(item: Int) returns Int effects [fs.read] { item }) }",
        r"export fn main() returns List<Int> { list.filter([1], fn(item: Int) returns Int { item }) }",
        r"export fn main() returns List<Int> { list.flat_map([1], fn(item: Int) returns Int { item }) }",
        r"export fn main() returns List<Int> { list.filter_map([1], fn(item: Int) returns Int { item }) }",
        r"export fn main() returns List<Int> { list.scan([1], 0, fn(item: Int) returns Int { item }) }",
    ] {
        let diagnostics = compile_source(source).expect_err("invalid callback is rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| matches!(diagnostic.code, "E2403" | "E3010" | "E3011")),
            "expected callback diagnostic, got {diagnostics:?}"
        );
    }
}

#[test]
fn direct_labeled_calls_reorder_after_source_order_evaluation() {
    let value = execute_source(
        r"
fn left() returns Int { 2 }
fn right() returns Int { 7 }
fn combine(first: Int, second: Int) returns Int { first * 10 + second }
export fn main() returns Int { combine(second: right(), first: left()) }
",
    );
    assert_eq!(value, Value::Int(27));

    for source in [
        r"fn combine(first: Int, second: Int) returns Int { first + second }
export fn main() returns Int { combine(first: 1, 2) }",
        r"fn combine(first: Int, second: Int) returns Int { first + second }
export fn main() returns Int { combine(first: 1, first: 2) }",
        r"fn combine(first: Int, second: Int) returns Int { first + second }
export fn main() returns Int { combine(first: 1) }",
        r"fn combine(first: Int, second: Int) returns Int { first + second }
export fn main() returns Int { combine(other: 1, second: 2) }",
        r"export fn main() returns Int {
  let callback = fn(value: Int) returns Int { value };
  callback(value: 1)
}",
    ] {
        let diagnostics = compile_source(source).expect_err("invalid labeled call is rejected");
        assert_eq!(diagnostics[0].code, "E3010", "{diagnostics:?}");
    }
}

#[test]
fn public_parameter_rename_changes_the_artifact_digest() {
    let first = compile_source(
        "export fn public_value(value: Int) returns Int { value }\nexport fn main() returns Int { public_value(1) }",
    )
        .expect("first public function compiles");
    let renamed = compile_source(
        "export fn public_value(other: Int) returns Int { other }\nexport fn main() returns Int { public_value(1) }",
    )
        .expect("renamed public function compiles");
    let first = assemble_loose_compilation("main.allen", first).expect("first artifact assembles");
    let renamed =
        assemble_loose_compilation("main.allen", renamed).expect("renamed artifact assembles");
    assert_ne!(
        encode(&first.artifact).expect("first artifact encodes"),
        encode(&renamed.artifact).expect("renamed artifact encodes"),
        "same-length public parameter renames are part of the artifact contract"
    );
}

#[test]
fn compiler_known_builtins_accept_canonical_labeled_arguments() {
    assert_eq!(
        execute_source(
            r#"export fn main() returns (List<Int>, String) {
  let values = list.filter(values: [1, 2, 3], callback: fn(item: Int) returns Bool { item > 1 });
  (values, string.replace(replacement: "b", value: "a-a", needle: "a"))
}"#,
        ),
        Value::Tuple(
            vec![
                Value::List(vec![Value::Int(2), Value::Int(3)].into()),
                Value::String("b-b".into()),
            ]
            .into()
        )
    );
    for source in [
        r"export fn main() returns List<Int> {
  list.filter(values: [1], callback: fn(item: Int) returns Bool { item > 0 }, extra: 1)
}",
        r#"export fn main() returns String {
  string.replace(value: "a", needle: "a", needle: "b")
}"#,
        r"export fn main() returns Int { length(value: [1], 2) }",
    ] {
        let diagnostics = compile_source(source).expect_err("invalid builtin labels are rejected");
        assert_eq!(diagnostics[0].code, "E3010", "{diagnostics:?}");
    }
}

#[test]
fn reordered_builtin_labels_evaluate_arguments_in_source_order() {
    let compilation = compile_source(
        r#"export fn main() returns String {
  string.replace(needle: fail("needle first"), value: fail("value second"), replacement: "x")
}"#,
    )
    .expect("reordered labeled builtin compiles");
    let error = execute(&verify(compilation.module).expect("bytecode verifies"))
        .expect_err("first source argument terminates execution");
    assert!(
        error.to_string().contains("needle first"),
        "unexpected error: {error}"
    );
}

#[test]
fn labeled_standard_and_tool_contract_calls_compile() {
    compile_source(
        r#"
export async fn main() returns Result<String, FileError> effects [fs.read] {
  await fs.read_text(path: "input.txt", workspace: fs.workspace())
}
"#,
    )
    .expect("standard operation accepts labels in a different source order");
    compile_source(
        r#"
export fn main() returns Bool effects [capability.inspect] {
  capability.is_granted(name: "fs.read")
}
"#,
    )
    .expect("capability inspection accepts its canonical label");

    let root = "pkg://labeled-tool@0.1.0/src/main.allen";
    let bundle = PackageSourceBundle {
        root: root.to_owned(),
        sources: BTreeMap::from([(
            root.to_owned(),
            r"
export async fn main(value: String) returns Result<String, String>
  effects [tool.example.echo@1] {
  await tools.example.echo.call(input: { value })
}
"
            .to_owned(),
        )]),
        import_targets: BTreeMap::new(),
        entry_points: vec![PackageEntryPoint {
            module: root.to_owned(),
            function: "main".to_owned(),
        }],
        entry_modules: Vec::new(),
    };
    let binding = CompilerToolBinding {
        source_path: vec!["example".to_owned(), "echo".to_owned()],
        contract: 0,
        input: ValueType::Record(vec![RecordField {
            name: "value".to_owned(),
            value_type: ValueType::String,
        }]),
        output: ValueType::String,
        declared_error: ValueType::String,
        error: ValueType::String,
        effect: "tool.example.echo@1".to_owned(),
        enum_types: Vec::new(),
    };
    compile_package_bundle_with_tools(&bundle, &[binding])
        .expect("typed tool contract accepts its input label");
}

#[test]
fn concise_lambdas_require_context_and_use_exact_callback_types() {
    assert_eq!(
        execute_source(
            r"
fn apply(callback: fn(Int) returns Int, value: Int) returns Int { callback(value) }
export fn main() returns Int { apply(fn(value) => value + 1, 4) }
",
        ),
        Value::Int(5)
    );
    assert_eq!(
        execute_source(
            r"export fn main() returns List<Int> {
  list.filter([1, 2, 3], fn(item) => item > 1)
}",
        ),
        Value::List(vec![Value::Int(2), Value::Int(3)].into())
    );
    let diagnostics = compile_source("export fn main() returns Int { fn(value) => value }")
        .expect_err("concise lambda without function context is rejected");
    assert_eq!(diagnostics[0].code, "E3011");
    assert_eq!(
        diagnostics[0].message,
        "concise lambda requires one exact expected function type"
    );
}

#[test]
fn option_question_returns_none_early_and_preserves_try_diagnostics() {
    let value = execute_source(
        r"
fn increment(value: Option<Int>) returns Option<Int> {
  let number = value?;
  Some(number + 1)
}
fn preserve_none(value: Option<Int>) returns Option<Int> {
  let number = value?;
  Some(number)
}
export fn main() returns (Option<Int>, Option<Int>, Option<Int>) {
  (increment(Some(1)), increment(None), preserve_none(None))
}
",
    );
    assert_eq!(
        value,
        Value::Tuple(vec![some(Value::Int(2)), some_none(), some_none()].into())
    );

    let async_value = execute_source(
        r"
async fn increment(value: Option<Int>) returns Option<Int> {
  let number = value?;
  Some(number + 1)
}
export async fn main() returns Option<Int> { await increment(Some(1)) }
",
    );
    assert_eq!(async_value, some(Value::Int(2)));

    for (source, message) in [
        (
            "export fn main() returns Option<Int> { 1? }",
            "'?' requires a Result or Option value",
        ),
        (
            "export fn main() returns Int { let value: Option<Int> = Some(1); value? }",
            "a function that uses '?' with Option must return Option",
        ),
    ] {
        let diagnostics = compile_source(source).expect_err("invalid Option try is rejected");
        assert_eq!(diagnostics[0].code, "E2017");
        assert_eq!(diagnostics[0].message, message);
    }

    let diagnostics = compile_source(
        r"
async fn number() returns Int { 1 }
export async fn main() returns Option<Int> effects [task.spawn] {
  let task = spawn number();
  let value: Option<Int> = None;
  value?
}
",
    )
    .expect_err("Option try cannot discard a live task");
    assert_eq!(diagnostics[0].code, "E3011");
    assert_eq!(
        diagnostics[0].message,
        "try none path would discard a live affine obligation"
    );
}
