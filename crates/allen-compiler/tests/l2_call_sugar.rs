use allen_bytecode::{Instruction, verify};
use allen_compiler::{
    PackageEntryPoint, PackageSourceBundle, compile_package_bundle, compile_source,
};
use allen_vm::{Value, execute};
use std::collections::BTreeMap;

fn execute_source(source: &str) -> Value {
    let compilation = compile_source(source).expect("L2 call-sugar source compiles");
    execute(&verify(compilation.module).expect("L2 call-sugar bytecode verifies"))
        .expect("L2 call-sugar source executes")
}

fn exported_default_digest(source: &str, function_name: &str, parameter: usize) -> [u8; 32] {
    let compilation = compile_source(source).expect("default digest source compiles");
    compilation
        .module
        .functions
        .iter()
        .find(|function| function.name.ends_with(&format!("::{function_name}")))
        .expect("exported function exists")
        .parameter_default_digests[parameter]
        .expect("parameter publishes a default digest")
}

#[test]
fn defaults_expand_in_declaration_order_for_positional_and_labeled_calls() {
    assert_eq!(
        execute_source(
            r"
fn encode(first: Int, second: Int = first + 1, third: Int = second + 1) returns Int {
  first * 100 + second * 10 + third
}
export fn main() returns Int { encode(1) + encode(first: 2, third: 9) }
",
        ),
        Value::Int(123 + 239)
    );
}

#[test]
fn invalid_default_shapes_and_effects_are_rejected() {
    for source in [
        "fn bad(first: Int = 1, second: Int) returns Int { first + second } export fn main() returns Int { bad(1, 2) }",
        "fn bad(first: Int = first) returns Int { first } export fn main() returns Int { bad() }",
        "fn bad(first: Int = second, second: Int = 2) returns Int { first } export fn main() returns Int { bad() }",
        "fn bad(path: String = fs.read_text(fs.workspace(), \"x\")) returns String { path } export fn main() returns String { bad() }",
    ] {
        assert!(
            compile_source(source).is_err(),
            "default must be rejected: {source}"
        );
    }
}

#[test]
fn trailing_callback_supplies_the_exact_final_parameter() {
    assert_eq!(
        execute_source(
            r"
fn apply(value: Int, callback: fn(Int) returns Int) returns Int { callback(value) }
export fn main() returns Int { apply(20) fn(value) => value + 22 }
",
        ),
        Value::Int(42)
    );
}

#[test]
fn placeholder_partials_capture_values_once_and_keep_holes_distinct() {
    assert_eq!(
        execute_source(
            r"
fn encode(first: Int, second: Int, third: Int) returns Int { first * 100 + second * 10 + third }
export fn main() returns Int {
  let partial: fn(Int, Int) returns Int = encode(_, 2, _);
  partial(4, 7)
}
",
        ),
        Value::Int(427)
    );
}

#[test]
fn namespace_partials_use_exact_builtin_signatures() {
    let partials = execute_source(
        r#"
export fn main() returns (String, List<Int>, Option<Int>, Option<Int>) {
  let replace: fn(String) returns String = string.replace(_, "old", "new");
  let append: fn(List<Int>) returns List<Int> = list.append(_, 3);
  let table = map { "answer": 42 };
  let lookup: fn(String) returns Option<Int> = map.get(table, _);
  let byte: fn(Bytes) returns Option<Int> = bytes.get(_, 1);
  (replace("old-old"), append([1, 2]), lookup("answer"), byte(b"xy"))
}
"#,
    );
    let direct = execute_source(
        r#"
export fn main() returns (String, List<Int>, Option<Int>, Option<Int>) {
  let table = map { "answer": 42 };
  (
    string.replace("old-old", "old", "new"),
    list.append([1, 2], 3),
    map.get(table, "answer"),
    bytes.get(b"xy", 1)
  )
}
"#,
    );
    assert_eq!(partials, direct);
}

#[test]
fn namespace_partial_supplied_values_are_captured_once_at_creation() {
    let compilation = compile_source(
        r#"
fn needle() returns String { "old" }
export fn main() returns String {
  let replace: fn(String) returns String = string.replace(_, needle(), "new");
  string.concat(replace("old"), replace("old"))
}
"#,
    )
    .expect("namespace partial capture source compiles");
    let entry = &compilation.module.functions[compilation.module.entry as usize];
    assert_eq!(
        entry
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::DirectCall { .. }))
            .count(),
        1,
        "the supplied direct call runs once when the closure is created"
    );
    assert_eq!(
        execute(&verify(compilation.module).expect("namespace partial verifies"))
            .expect("namespace partial executes"),
        Value::String("newnew".to_owned().into())
    );
}

#[test]
fn namespace_partials_reject_unknown_or_mismatched_exact_types() {
    for source in [
        "export fn main() returns fn(Int) returns Option<Int> { list.get(_, 0) }",
        "export fn main() returns fn(Bytes) returns Option<Int> { bytes.get(_, \"wrong\") }",
    ] {
        assert!(
            compile_source(source).is_err(),
            "invalid namespace partial must be rejected: {source}"
        );
    }
}

#[test]
fn effectful_namespace_partials_publish_the_builtin_effect() {
    compile_source(
        r#"
export fn main() returns fn(Workspace) returns Future<Result<String, FileError>> effects [fs.read]
effects [fs.read] {
  fs.read_text(_, "input.txt")
}
"#,
    )
    .expect("effectful namespace partial has the exact callable effect");

    assert!(
        compile_source(
            r#"
export fn main() returns fn(Workspace) returns Future<Result<String, FileError>> {
  fs.read_text(_, "input.txt")
}
"#,
        )
        .is_err(),
        "dropping the builtin effect from the function type must fail"
    );
}

#[test]
fn optional_call_type_error_points_at_the_chain_operator() {
    let source = "export fn main() returns Int { let value = 1; value?.missing() }";
    let diagnostics = compile_source(source).expect_err("non-Option optional call is rejected");
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("optional chain requires an Option")
        })
        .expect("optional-chain type diagnostic exists");
    let start = source
        .find("?.")
        .expect("source contains optional-chain operator");
    assert_eq!(
        diagnostic.span,
        allen_compiler::Span {
            start,
            end: start + 2,
        }
    );
}

#[test]
fn composition_and_pipe_match_explicit_nested_calls() {
    assert_eq!(
        execute_source(
            r"
fn add(value: Int, amount: Int) returns Int { value + amount }
fn twice(value: Int) returns Int { value * 2 }
export fn main() returns Int {
  let add_two: fn(Int) returns Int = add(_, 2);
  let composed = add_two >> twice;
  composed(3) + (3 |> add(_, 4) |> twice())
}
",
        ),
        Value::Int(24)
    );
}

#[test]
fn function_values_still_require_exact_arity_despite_defaults() {
    let diagnostics = compile_source(
        r"
fn add(value: Int, amount: Int = 1) returns Int { value + amount }
export fn main() returns Int {
  let callback: fn(Int, Int) returns Int = add;
  callback(1)
}
",
    )
    .expect_err("function-value calls do not expand defaults");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "E3010")
    );
}

#[test]
fn compiler_owned_extension_namespaces_insert_the_receiver_once() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns (List<Int>, String, Option<Int>, Option<Int>) {
  let values = [1, 2].map(fn(value: Int) returns Int { value * 10 });
  let table = map { "answer": 42 };
  (values, "a-a".replace(needle: "a", replacement: "b"), table.get("answer"), b"xy".get(1))
}
"#,
        ),
        execute_source(
            r#"
export fn main() returns (List<Int>, String, Option<Int>, Option<Int>) {
  let values = list.map([1, 2], fn(value: Int) returns Int { value * 10 });
  let table = map { "answer": 42 };
  (values, string.replace("a-a", "a", "b"), map.get(table, "answer"), bytes.get(b"xy", 1))
}
"#,
        )
    );
}

#[test]
fn only_explicit_extension_imports_enter_member_lookup() {
    let bundle = |keyword: &str| PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            (
                "main.allen".to_owned(),
                format!(
                    "import {keyword} {{ bump }} from \"./extension.allen\"; export fn main() returns Int {{ (41).bump() }}"
                ),
            ),
            (
                "extension.allen".to_owned(),
                "export fn bump(value: Int) returns Int { value + 1 }".to_owned(),
            ),
        ]),
        import_targets: BTreeMap::new(),
        entry_modules: vec!["main.allen".to_owned()],
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: "main".to_owned(),
        }],
    };
    let compiled =
        compile_package_bundle(&bundle("extension")).expect("explicit extension import resolves");
    assert_eq!(
        execute(&verify(compiled.module).expect("extension bytecode verifies"))
            .expect("extension executes"),
        Value::Int(42)
    );
    assert!(
        compile_package_bundle(&bundle("")).is_err(),
        "ordinary imports do not enter extension lookup"
    );
}

#[test]
fn extension_aliases_ambiguity_and_field_precedence_are_deterministic() {
    let alias = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            (
                "main.allen".to_owned(),
                "import extension { bump as advance } from \"./extension.allen\"; export fn main() returns Int { (41).advance() }".to_owned(),
            ),
            (
                "extension.allen".to_owned(),
                "export fn bump(value: Int) returns Int { value + 1 }".to_owned(),
            ),
        ]),
        import_targets: BTreeMap::new(),
        entry_modules: vec!["main.allen".to_owned()],
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: "main".to_owned(),
        }],
    };
    let compiled = compile_package_bundle(&alias).expect("extension alias resolves");
    assert_eq!(
        execute(&verify(compiled.module).expect("alias bytecode verifies"))
            .expect("alias executes"),
        Value::Int(42)
    );

    let field_wins = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            (
                "main.allen".to_owned(),
                r#"
import extension { bump } from "./extension.allen";
record Holder { bump: fn(Int) returns Int }
export fn main() returns Int {
  let holder = Holder { bump: fn(value: Int) returns Int { value + 1 } };
  holder.bump(41)
}
"#
                .to_owned(),
            ),
            (
                "extension.allen".to_owned(),
                "export fn bump<T: Eq>(value: T, amount: Int) returns Int { amount + 100 }"
                    .to_owned(),
            ),
        ]),
        import_targets: BTreeMap::new(),
        entry_modules: vec!["main.allen".to_owned()],
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: "main".to_owned(),
        }],
    };
    let compiled = compile_package_bundle(&field_wins).expect("real function field wins");
    assert_eq!(
        execute(&verify(compiled.module).expect("field bytecode verifies"))
            .expect("field call executes"),
        Value::Int(42)
    );

    let ambiguous = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            (
                "main.allen".to_owned(),
                "import extension { first as bump } from \"./one.allen\"; import extension { second as bump } from \"./two.allen\"; export fn main() returns Int { (1).bump() }".to_owned(),
            ),
            (
                "one.allen".to_owned(),
                "export fn first(value: Int) returns Int { value }".to_owned(),
            ),
            (
                "two.allen".to_owned(),
                "export fn second(value: Int) returns Int { value }".to_owned(),
            ),
        ]),
        import_targets: BTreeMap::new(),
        entry_modules: vec!["main.allen".to_owned()],
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: "main".to_owned(),
        }],
    };
    let diagnostics = compile_package_bundle(&ambiguous).expect_err("ambiguity is rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("ambiguous"))
    );
}

#[test]
fn extension_receiver_expression_is_lowered_once() {
    let compilation = compile_source(
        r"
fn values() returns List<Int> { [1, 2] }
export fn main() returns List<Int> {
  values().map(fn(value: Int) returns Int { value + 1 })
}
",
    )
    .expect("extension receiver source compiles");
    let entry = &compilation.module.functions[compilation.module.entry as usize];
    assert_eq!(
        entry
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::DirectCall { .. }))
            .count(),
        1,
        "receiver-producing direct call is emitted once"
    );
}

#[test]
fn defaults_apply_across_imports_and_publish_exact_source_digests() {
    let canonical = exported_default_digest(
        "export fn add(value: Int, amount: Int = 1) returns Int { value + amount } export fn main() returns Int { add(41) }",
        "add",
        1,
    );
    let trivia_only = exported_default_digest(
        "export fn add(value: Int, amount: Int = /* comment */\n  1) returns Int { value + amount } export fn main() returns Int { add(41) }",
        "add",
        1,
    );
    let token_change = exported_default_digest(
        "export fn add(value: Int, amount: Int = 2) returns Int { value + amount } export fn main() returns Int { add(40) }",
        "add",
        1,
    );
    assert_eq!(
        canonical, trivia_only,
        "whitespace and comments do not alter the artifact default contract"
    );
    assert_ne!(
        canonical, token_change,
        "a default-expression token change alters the artifact default contract"
    );

    let imported = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            (
                "main.allen".to_owned(),
                "import { add } from \"./support.allen\"; export fn main() returns Int { add(41) }"
                    .to_owned(),
            ),
            (
                "support.allen".to_owned(),
                "export fn add(value: Int, amount: Int = 1) returns Int { value + amount }"
                    .to_owned(),
            ),
        ]),
        import_targets: BTreeMap::new(),
        entry_modules: vec!["main.allen".to_owned()],
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: "main".to_owned(),
        }],
    };
    let compiled = compile_package_bundle(&imported).expect("imported default expands");
    assert_eq!(
        execute(&verify(compiled.module).expect("imported default verifies"))
            .expect("imported default executes"),
        Value::Int(42)
    );
}
