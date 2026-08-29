#![allow(clippy::needless_raw_string_hashes)]

use allen_bytecode::verify;
use allen_compiler::compile_source;
use allen_vm::{EnumIdentity, EnumPayload, EnumValue};
use allen_vm::{Value, execute};
use std::rc::Rc;

fn execute_source(source: &str) -> Value {
    let compilation = compile_source(source).expect("L2 data-flow source compiles");
    execute(&verify(compilation.module).expect("L2 data-flow bytecode verifies"))
        .expect("L2 data-flow source executes")
}

#[test]
fn list_and_map_spreads_preserve_source_order() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns (List<Int>, Map<String, Int>) {
  ([1, ..[2, 3], 4], map { "a": 1, ..map { "a": 2, "b": 3 }, "b": 4 })
}
"#,
        ),
        Value::Tuple(
            vec![
                Value::List(
                    vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)].into()
                ),
                Value::Map(
                    vec![
                        (Value::String("a".to_owned().into()), Value::Int(2)),
                        (Value::String("b".to_owned().into()), Value::Int(4)),
                    ]
                    .into()
                ),
            ]
            .into()
        )
    );
}

#[test]
fn record_update_reuses_base_and_replaces_fields() {
    assert_eq!(
        execute_source(
            r#"
record Pair { left: Int, right: Int }
fn make() returns Pair { Pair { left: 1, right: 2 } }
export fn main() returns Int {
  let pair = make();
  let updated = Pair { ..pair, right: 7 };
  updated.left + updated.right
}
"#,
        ),
        Value::Int(8)
    );
}

#[test]
fn optional_field_returns_none_or_some() {
    assert_eq!(
        execute_source(
            r#"
record Pair { left: Int }
fn get(value: Option<Pair>) returns Option<Int> { value?.left }
export fn main() returns (Option<Int>, Option<Int>) {
  (get(Some(Pair { left: 42 })), get(None))
}
"#,
        ),
        Value::Tuple(
            vec![
                Value::Enum(Rc::new(EnumValue {
                    identity: EnumIdentity::Option,
                    type_name: "Option".into(),
                    variant: 1,
                    variant_name: "Some".into(),
                    payload: EnumPayload::Tuple(vec![Value::Int(42)].into()),
                })),
                Value::Enum(Rc::new(EnumValue {
                    identity: EnumIdentity::Option,
                    type_name: "Option".into(),
                    variant: 0,
                    variant_name: "None".into(),
                    payload: EnumPayload::Unit,
                })),
            ]
            .into()
        )
    );
}

#[test]
fn optional_extension_call_skips_arguments_on_none() {
    assert_eq!(
        execute_source(
            r#"
fn transform(value: Option<List<Int>>) returns Option<List<Int>> {
  value?.map(fn(item: Int) returns Int { item + 1 })
}
export fn main() returns Option<List<Int>> { transform(Some([1, 2])) }
"#,
        ),
        Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::Option,
            type_name: "Option".into(),
            variant: 1,
            variant_name: "Some".into(),
            payload: EnumPayload::Tuple(
                vec![Value::List(vec![Value::Int(2), Value::Int(3)].into())].into(),
            ),
        }))
    );
}

#[test]
fn optional_chain_is_local_in_a_non_option_function() {
    assert_eq!(
        execute_source(
            r#"
record Box { field: String }
export fn main() returns Int {
  let value: Option<String> = Some(Box { field: "ready" })?.field;
  length(value ?? "")
}
"#,
        ),
        Value::Int(5)
    );
}

#[test]
fn optional_chain_skips_later_arguments_and_flattens_each_step() {
    assert_eq!(
        execute_source(
            r#"
record Inner { value: Int }
record Outer { inner: Option<Inner> }
fn chain(value: Option<Outer>) returns Option<Int> { value?.inner?.value }
record Handler { callback: fn(Int) returns Int }
fn invoke(value: Option<Handler>) returns Option<Int> {
  value?.callback(1 / 0)
}
export fn main() returns (Option<Int>, Option<Int>) {
  (chain(Some(Outer { inner: Some(Inner { value: 41 }) })), invoke(None))
}
"#,
        ),
        Value::Tuple(
            vec![
                Value::Enum(Rc::new(EnumValue {
                    identity: EnumIdentity::Option,
                    type_name: "Option".into(),
                    variant: 1,
                    variant_name: "Some".into(),
                    payload: EnumPayload::Tuple(vec![Value::Int(41)].into()),
                })),
                Value::Enum(Rc::new(EnumValue {
                    identity: EnumIdentity::Option,
                    type_name: "Option".into(),
                    variant: 0,
                    variant_name: "None".into(),
                    payload: EnumPayload::Unit,
                })),
            ]
            .into()
        )
    );
}

#[test]
fn spread_and_record_update_types_remain_exact() {
    for source in [
        r#"export fn main() returns List<Int> { [1, ..[true]] }"#,
        r#"export fn main() returns Map<String, Int> { map { "a": 1, ..map { 2: 3 } } }"#,
        r#"record Pair { left: Int, right: Int } export fn main() returns Pair { Pair { ..Pair { left: 1, right: 2 }, left: 3, left: 4 } }"#,
    ] {
        assert!(
            compile_source(source).is_err(),
            "invalid L2 data flow must fail"
        );
    }
}
