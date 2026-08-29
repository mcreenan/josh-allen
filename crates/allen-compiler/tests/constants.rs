use allen_bytecode::{Instruction, encode, verify};
use allen_compiler::{
    PackageEntryPoint, PackageSourceBundle, assemble_loose_compilation, compile_package_bundle,
    compile_source,
};
use allen_vm::{Value, execute};
use std::collections::BTreeMap;

fn execute_source(source: &str) -> Value {
    let compilation = compile_source(source).expect("constant source compiles");
    execute(&verify(compilation.module).expect("constant bytecode verifies"))
        .expect("constant source executes")
}

#[test]
fn constants_forward_reference_and_materialize_without_runtime_initializers() {
    let source = r"
        const answer: Int = base + 2;
        const base: Int = 40;
        export fn main() returns (Int, List<Int>, Option<Int>) {
            (answer, [base, answer], Some(answer))
        }
    ";
    let compilation = compile_source(source).expect("forward constants compile");
    let repeated = compile_source(source).expect("repeated compile succeeds");
    assert_eq!(compilation.module, repeated.module);
    assert_eq!(compilation.hir, repeated.hir);
    assert_eq!(compilation.mir, repeated.mir);
    assert_eq!(compilation.module.functions.len(), 1);
    assert!(
        compilation.module.functions[0]
            .code
            .iter()
            .all(|instruction| !matches!(instruction, Instruction::IntBinary { .. })),
        "constant arithmetic must not survive into runtime bytecode"
    );
    let first_artifact = assemble_loose_compilation("main.allen", compilation.clone())
        .expect("constant artifact assembles");
    let second_artifact = assemble_loose_compilation("main.allen", repeated)
        .expect("repeated constant artifact assembles");
    assert_eq!(
        encode(&first_artifact.artifact).expect("constant artifact encodes"),
        encode(&second_artifact.artifact).expect("repeated constant artifact encodes")
    );
    let value =
        execute(&verify(compilation.module).expect("bytecode verifies")).expect("source executes");
    assert_eq!(
        value,
        Value::Tuple(
            vec![
                Value::Int(42),
                Value::List(vec![Value::Int(40), Value::Int(42)].into()),
                Value::Enum(std::rc::Rc::new(allen_vm::EnumValue {
                    identity: allen_vm::EnumIdentity::Option,
                    type_name: "Option".into(),
                    variant: 1,
                    variant_name: "Some".into(),
                    payload: allen_vm::EnumPayload::Tuple(vec![Value::Int(42)].into()),
                })),
            ]
            .into()
        )
    );
}

#[test]
fn constants_reject_cycles_calls_and_type_mismatches() {
    let cycle_source = "const b: Int = a; const a: Int = b; export fn main() returns Int { a }";
    let cycle = compile_source(cycle_source).expect_err("cycle is rejected");
    assert_eq!(
        cycle,
        compile_source(cycle_source).expect_err("cycle is deterministic")
    );
    assert_eq!(cycle[0].code, "E3012");
    assert!(cycle[0].message.contains("main.allen::a, main.allen::b"));

    let call = compile_source(
        "fn runtime() returns Int { 1 } const a: Int = runtime(); export fn main() returns Int { a }",
    )
    .expect_err("runtime calls are rejected");
    assert!(call[0].message.contains("cannot call runtime functions"));

    let mismatch = compile_source("const a: String = 1; export fn main() returns String { a }")
        .expect_err("declared type is checked");
    assert_eq!(mismatch[0].code, "E3007");

    for collision in [
        "const same: Int = 1; fn same() returns Int { 2 } export fn main() returns Int { same }",
        "record same { value: Int } const same: Int = 1; export fn main() returns Int { same }",
        "newtype same = Int const same: Int = 1; export fn main() returns Int { same }",
    ] {
        let diagnostic = compile_source(collision).expect_err("value namespace collision");
        assert_eq!(diagnostic[0].code, "E3005");
        assert!(diagnostic[0].message.contains("duplicate value 'same'"));
    }

    assert_eq!(
        execute_source(
            "const clean: String = string.replace(\"a-b\", \"-\", \"\"); export fn main() returns String { clean }"
        ),
        Value::String("ab".into())
    );
}

#[test]
fn exported_constants_import_by_the_existing_qualified_module_contract() {
    let bundle = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            (
                "main.allen".to_owned(),
                "import { answer } from \"./values.allen\"; export fn main() returns Int { answer }"
                    .to_owned(),
            ),
            (
                "values.allen".to_owned(),
                "export const answer: Int = 6 * 7;".to_owned(),
            ),
        ]),
        import_targets: BTreeMap::new(),
        entry_modules: vec!["main.allen".to_owned()],
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: "main".to_owned(),
        }],
    };
    let compilation = compile_package_bundle(&bundle).expect("exported constant imports");
    assert_eq!(
        execute(&verify(compilation.module).expect("bytecode verifies")).expect("executes"),
        Value::Int(42)
    );

    assert_eq!(
        execute_source("const value: Int = 42; export fn main() returns Int { value }"),
        Value::Int(42)
    );

    let nominal_bundle = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            (
                "main.allen".to_owned(),
                "import { Epoch, start } from \"./values.allen\"; export fn main() returns Epoch { start }"
                    .to_owned(),
            ),
            (
                "values.allen".to_owned(),
                "export newtype Epoch = Int export const start: Epoch = Epoch(7);".to_owned(),
            ),
        ]),
        import_targets: BTreeMap::new(),
        entry_modules: vec!["main.allen".to_owned()],
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: "main".to_owned(),
        }],
    };
    let value = execute(
        &verify(
            compile_package_bundle(&nominal_bundle)
                .expect("nominal constant imports")
                .module,
        )
        .expect("nominal bytecode verifies"),
    )
    .expect("nominal constant executes");
    let Value::Newtype(value) = value else {
        panic!("expected imported newtype constant")
    };
    assert_eq!(value.identity(), "values.allen::Epoch");
}

#[test]
fn constants_preserve_aggregate_and_nominal_values() {
    let source = r#"
        record Config { label: String, values: List<Int> }
        enum Reading { Named { label: String, value: Int } }
        newtype Epoch = Int
        const epoch: Epoch = Epoch(7);
        const config: Config = Config { values: [1, 2], label: "cpu" };
        const reading: Reading = Reading.Named { value: 9, label: "cpu" };
        const lookup: Map<String, Int> = map { "b": 2, "a": 1 };
        export fn main() returns (Epoch, Config, Reading, Map<String, Int>) {
            (epoch, config, reading, lookup)
        }
    "#;
    let compilation = compile_source(source).expect("aggregate constants compile");
    assert_eq!(compilation.hir.modules[0].constants.len(), 4);
    assert_eq!(compilation.mir.constants.len(), 4);
    assert_eq!(compilation.module.functions.len(), 1);
    let value = execute(&verify(compilation.module).expect("bytecode verifies"))
        .expect("aggregate constants execute");
    let rendered = value.to_string();
    assert!(rendered.contains("Epoch(7)"));
    assert!(rendered.contains("Reading.Named"));
    assert!(rendered.contains("map {\"a\": 1, \"b\": 2}"));
}

#[test]
fn constants_are_pure_private_and_resource_bounded() {
    let terminal =
        compile_source("const value: Int = fail(\"no\"); export fn main() returns Int { value }")
            .expect_err("terminal expressions are rejected");
    assert!(terminal[0].message.contains("runtime-only expression"));

    for source in [
        "const value: Int = if (true) { 1 } else { 2 }; export fn main() returns Int { value }",
        "const value: Int = if (true) { for item in [1] { } 1 } else { 2 }; export fn main() returns Int { value }",
    ] {
        let non_constant = compile_source(source).expect_err("control flow is not constant syntax");
        assert_eq!(non_constant[0].code, "E3011");
        assert!(non_constant[0].message.contains("non-constant expression"));
    }

    let private_bundle = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            (
                "main.allen".to_owned(),
                "import { hidden } from \"./values.allen\"; export fn main() returns Int { hidden }"
                    .to_owned(),
            ),
            ("values.allen".to_owned(), "const hidden: Int = 1;".to_owned()),
        ]),
        import_targets: BTreeMap::new(),
        entry_modules: vec!["main.allen".to_owned()],
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: "main".to_owned(),
        }],
    };
    let private = compile_package_bundle(&private_bundle).expect_err("private constant import");
    assert_eq!(private[0].code, "E3003");

    let element = format!("\"{}\"", "x".repeat(80));
    let elements = std::iter::repeat_n(element.as_str(), 11_000)
        .collect::<Vec<_>>()
        .join(",");
    let source = format!(
        "const too_large: List<String> = [{elements}]; export fn main() returns Int {{ 0 }}"
    );
    let bounded = compile_source(&source).expect_err("constant allocation is bounded");
    assert_eq!(bounded[0].code, "E3011");
    assert!(bounded[0].message.contains("resource.limit"));
}
