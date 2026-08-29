#![allow(clippy::needless_raw_string_hashes)]

use allen_bytecode::{Instruction, encode, verify};
use allen_compiler::{
    PackageEntryPoint, PackageSourceBundle, assemble_loose_compilation, compile_package_bundle,
    compile_source,
};
use allen_vm::{Value, execute};
use std::collections::BTreeMap;

fn execute_source(source: &str) -> Value {
    let compilation = compile_source(source).expect("local-function source compiles");
    execute(&verify(compilation.module).expect("local-function bytecode verifies"))
        .expect("local-function source executes")
}

fn compile_error(source: &str) -> allen_compiler::Diagnostic {
    compile_source(source)
        .expect_err("source must be rejected")
        .into_iter()
        .next()
        .expect("compiler returns one diagnostic")
}

#[test]
fn local_functions_support_ordinary_call_sugar_and_function_values() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns (Int, Int, Int, Int, Int, Int) {
  fn add(left: Int, right: Int = left + 1) returns Int { left + right }
  fn apply(value: Int, callback: fn(Int) returns Int) returns Int { callback(value) }
  let partial: fn(Int) returns Int = add(_, 2);
  let value: fn(Int, Int) returns Int = add;
  (
    add(20),
    add(right: 2, left: 40),
    40 |> add(_, 2),
    partial(40),
    value(20, 22),
    apply(41) fn(input: Int) returns Int { input + 1 }
  )
}
"#,
        ),
        Value::Tuple(
            vec![
                Value::Int(41),
                Value::Int(42),
                Value::Int(42),
                Value::Int(42),
                Value::Int(42),
                Value::Int(42),
            ]
            .into()
        )
    );
}

#[test]
fn local_bodies_can_use_constants_imports_and_top_level_functions() {
    let bundle = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            (
                "main.allen".to_owned(),
                r#"
import { imported, amount } from "./values.allen";
const local_amount: Int = 1;
fn top(value: Int) returns Int { value + local_amount }
export fn main() returns Int {
  fn calculate(value: Int) returns Int { imported(top(value)) + amount }
  calculate(39)
}
"#
                .to_owned(),
            ),
            (
                "values.allen".to_owned(),
                "export const amount: Int = 1; export fn imported(value: Int) returns Int { value + 1 }"
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
    let compilation = compile_package_bundle(&bundle).expect("local imports compile");
    assert_eq!(
        execute(&verify(compilation.module).expect("local import bytecode verifies"))
            .expect("local import source executes"),
        Value::Int(42)
    );
}

#[test]
fn nested_and_disjoint_local_scopes_restore_exactly() {
    assert_eq!(
        execute_source(
            r#"
const answer: Int = 42;
export fn main() returns (Int, Int, Int) {
  fn outer() returns Int {
    fn inner() returns Int { answer }
    inner()
  }
  let left = if (true) { fn choose() returns Int { 1 } choose() } else { 0 };
  let right = if (true) { fn choose() returns Int { 2 } choose() } else { 0 };
  (outer(), left, right)
}
"#,
        ),
        Value::Tuple(vec![Value::Int(42), Value::Int(1), Value::Int(2)].into())
    );

    let leak = compile_error(
        r#"
export fn main() returns Int {
  let value = if (true) { fn hidden() returns Int { 1 } hidden() } else { 0 };
  hidden()
}
"#,
    );
    assert_eq!(leak.code, "E3005");
    assert!(leak.message.contains("unknown"));
}

#[test]
fn declaration_order_recursion_and_local_body_isolation_are_rejected() {
    for source in [
        r#"export fn main() returns Int { let value = later(); fn later() returns Int { 1 } value }"#,
        r#"export fn main() returns Int { fn recurse() returns Int { recurse() } recurse() }"#,
        r#"export fn main() returns Int { fn first() returns Int { second() } fn second() returns Int { first() } first() }"#,
        r#"export fn main() returns Int { fn first() returns Int { 1 } fn second() returns Int { first() } second() }"#,
    ] {
        let diagnostic = compile_error(source);
        assert_eq!(diagnostic.code, "E3005", "{source}\n{diagnostic:?}");
        assert!(diagnostic.message.contains("unknown"), "{diagnostic:?}");
    }
}

#[test]
fn captures_name_the_enclosing_binding_and_local_function() {
    for (source, binding) in [
        (
            r#"export fn main() returns Int { let value = 1; fn read() returns Int { value } read() }"#,
            "value",
        ),
        (
            r#"export fn main() returns Int { mut state = 1; fn read() returns Int { state } read() }"#,
            "state",
        ),
        (
            r#"fn outer(future: Future<Int>) returns Int { fn read() returns Future<Int> { future } 0 } export fn main() returns Int { 0 }"#,
            "future",
        ),
        (
            r#"export fn main() returns Int { let amount = 1; fn add(value: Int = amount) returns Int { value } add() }"#,
            "amount",
        ),
    ] {
        let diagnostic = compile_error(source);
        assert_eq!(diagnostic.code, "E3011", "{diagnostic:?}");
        assert!(diagnostic.message.contains(binding), "{diagnostic:?}");
        assert!(
            diagnostic.message.contains("read") || diagnostic.message.contains("add"),
            "{diagnostic:?}"
        );
    }
}

#[test]
fn local_names_cannot_shadow_or_be_shadowed() {
    for source in [
        r#"export fn main() returns Int { let chosen = 1; fn chosen() returns Int { 2 } chosen }"#,
        r#"export fn main() returns Int { fn chosen() returns Int { 1 } let chosen = 2; chosen }"#,
        r#"fn chosen() returns Int { 1 } export fn main() returns Int { fn chosen() returns Int { 2 } chosen() }"#,
        r#"export fn main() returns Int { fn chosen() returns Int { fn chosen() returns Int { 1 } chosen() } chosen() }"#,
        r#"export fn main() returns Int { fn chosen() returns Int { 1 } let callback = fn(chosen: Int) returns Int { chosen }; callback(2) }"#,
    ] {
        let diagnostic = compile_error(source);
        assert_eq!(diagnostic.code, "E3005", "{source}\n{diagnostic:?}");
        assert!(diagnostic.message.contains("chosen"), "{diagnostic:?}");
    }
}

#[test]
fn local_types_defaults_and_effects_use_ordinary_rules() {
    for (source, code) in [
        (
            r#"export fn main() returns Int { fn wrong() returns Int { "no" } wrong() }"#,
            "E3007",
        ),
        (
            r#"export fn main() returns Int { fn wrong(first: Int = 1, second: Int) returns Int { first + second } wrong(2) }"#,
            "E3010",
        ),
        (
            r#"export fn main() returns Int { fn wrong(first: Int = second, second: Int = 1) returns Int { first + second } wrong() }"#,
            "E3005",
        ),
        (
            r#"export fn main() returns Bool effects [capability.inspect] { fn inspect() returns Bool { capability.is_granted("fs.read") } inspect() }"#,
            "E2403",
        ),
    ] {
        let diagnostic = compile_error(source);
        assert_eq!(diagnostic.code, code, "{source}\n{diagnostic:?}");
    }

    let source = r#"
export fn main() returns Bool effects [capability.inspect] {
  fn inspect() returns Bool effects [capability.inspect] {
    capability.is_granted("fs.read")
  }
  inspect()
}
"#;
    let compilation = compile_source(source).expect("declared local effects compile");
    verify(compilation.module).expect("declared local effects verify");
}

#[test]
fn effectful_local_call_paths_require_an_enclosing_contract() {
    for source in [
        r#"
export fn main() returns Bool {
  fn inspect() returns Bool effects [capability.inspect] {
    capability.is_granted("fs.read")
  }
  inspect()
}
"#,
        r#"
export fn main() returns Bool {
  fn inspect(name: String = "fs.read") returns Bool effects [capability.inspect] {
    capability.is_granted(name)
  }
  inspect()
}
"#,
        r#"
fn keep(callback: fn() returns Bool effects [capability.inspect]) returns Bool { true }
export fn main() returns Bool {
  fn inspect() returns Bool effects [capability.inspect] {
    capability.is_granted("fs.read")
  }
  keep(inspect)
}
"#,
        r#"
export fn main() returns Bool {
  fn inspect(name: String) returns Bool effects [capability.inspect] {
    capability.is_granted(name)
  }
  let callback: fn(String) returns Bool effects [capability.inspect] = inspect(_);
  callback("fs.read")
}
"#,
        r#"
export fn main() returns Bool {
  fn apply(
    value: String,
    callback: fn(String) returns Bool effects [capability.inspect]
  ) returns Bool effects [capability.inspect] {
    callback(value)
  }
  apply("fs.read") fn(name: String) returns Bool effects [capability.inspect] {
    capability.is_granted(name)
  }
}
"#,
    ] {
        let diagnostic = compile_error(source);
        assert_eq!(diagnostic.code, "E2403", "{source}\n{diagnostic:?}");
        assert!(
            diagnostic.message.contains("main")
                && diagnostic.message.contains("capability.inspect"),
            "{source}\n{diagnostic:?}"
        );
    }
}

#[test]
fn explicit_enclosing_effect_contract_accepts_local_call_paths() {
    let source = r#"
fn keep(callback: fn(String) returns Bool effects [capability.inspect]) returns Bool { true }
export fn main() returns (Bool, Bool, Bool, Bool) effects [capability.inspect] {
  fn inspect(name: String = "fs.read") returns Bool effects [capability.inspect] {
    capability.is_granted(name)
  }
  fn apply(
    value: String,
    callback: fn(String) returns Bool effects [capability.inspect]
  ) returns Bool effects [capability.inspect] {
    callback(value)
  }
  let partial: fn(String) returns Bool effects [capability.inspect] = inspect(_);
  (
    inspect(),
    partial("fs.read"),
    keep(inspect),
    apply("fs.read") fn(name: String) returns Bool effects [capability.inspect] {
      capability.is_granted(name)
    }
  )
}
"#;
    let compilation = compile_source(source).expect("explicit enclosing effects compile");
    verify(compilation.module).expect("explicit enclosing effects verify");
}

#[test]
fn local_artifacts_are_deterministic_private_and_capture_free() {
    let source = r#"
export fn main() returns Int {
  fn increment(value: Int, amount: Int = 1) returns Int { value + amount }
  let callback: fn(Int, Int) returns Int = increment;
  increment(callback(40, 0))
}
"#;
    let first = compile_source(source).expect("first local compilation succeeds");
    let second = compile_source(source).expect("second local compilation succeeds");
    assert_eq!(first.module, second.module);
    assert_eq!(first.hir, second.hir);
    assert_eq!(first.mir, second.mir);
    assert_eq!(first.exported_functions.len(), 1);
    assert_eq!(first.exported_functions[0].function, "main");

    let local_hir = first
        .hir
        .modules
        .iter()
        .flat_map(|module| &module.functions)
        .find(|function| function.name.starts_with("$local@"))
        .expect("private local HIR function exists");
    let local_id = first.module.functions.iter().position(|function| {
        function
            .name
            .ends_with(&format!("$closure@{}", local_hir.symbol))
    });
    let local_id = u32::try_from(local_id.expect("private bytecode function exists"))
        .expect("function ID fits");
    assert!(
        first.module.functions[local_id as usize]
            .captures
            .is_empty()
    );
    assert_eq!(
        first.module.functions[local_id as usize]
            .parameter_default_digests
            .iter()
            .map(Option::is_some)
            .collect::<Vec<_>>(),
        vec![false, true]
    );
    let entry = &first.module.functions[first.module.entry as usize];
    assert!(entry.code.iter().any(
        |instruction| matches!(instruction, Instruction::DirectCall { function, .. } if *function == local_id)
    ));
    assert!(entry.code.iter().any(
        |instruction| matches!(instruction, Instruction::ClosureNew { function, captures, .. } if *function == local_id && captures.is_empty())
    ));

    let first_artifact =
        assemble_loose_compilation("main.allen", first).expect("first local artifact assembles");
    let second_artifact =
        assemble_loose_compilation("main.allen", second).expect("second local artifact assembles");
    assert_eq!(
        encode(&first_artifact.artifact).expect("first local artifact encodes"),
        encode(&second_artifact.artifact).expect("second local artifact encodes")
    );
}
