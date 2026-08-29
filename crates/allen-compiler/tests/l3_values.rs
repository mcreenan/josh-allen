#![allow(clippy::needless_raw_string_hashes)]

use allen_bytecode::verify;
use allen_compiler::{
    PackageEntryPoint, PackageSourceBundle, assemble_loose_compilation, compile_package_bundle,
    compile_source,
};
use allen_vm::{EnumIdentity, EnumPayload, EnumValue, Value, execute};
use std::collections::BTreeMap;
use std::rc::Rc;

fn execute_source(source: &str) -> Value {
    let compilation = compile_source(source).expect("L3 value source compiles");
    execute(&verify(compilation.module).expect("L3 value bytecode verifies"))
        .expect("L3 value source executes")
}

#[test]
fn range_values_compare_all_fields_and_drive_arbitrary_for_sources() {
    assert_eq!(
        execute_source(
            r#"
fn bounds() returns Range<Int> { 1..=3 }
export fn main() returns (Bool, Bool, Bool, Int, Int) {
  mut inclusive_total = 0;
  for value in bounds() { inclusive_total += value; }
  mut empty_total = 0;
  for value in 3..=1 { empty_total += value; }
  (
    (1..3) == (1..3),
    (1..3) == (1..=3),
    (1..3) == (2..3),
    inclusive_total,
    empty_total
  )
}
"#,
        ),
        Value::Tuple(
            vec![
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(false),
                Value::Int(6),
                Value::Int(0),
            ]
            .into()
        )
    );
}

#[test]
fn inclusive_range_at_int_max_does_not_increment_past_the_endpoint() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns Int {
  mut visits = 0;
  for value in 9223372036854775807..=9223372036854775807 {
    visits += 1;
  }
  visits
}
"#,
        ),
        Value::Int(1)
    );
}

#[test]
fn slices_cover_lists_bytes_strings_and_invalid_bounds() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns (List<Int>, Bytes, String, List<Int>) {
  let empty: List<Int> = [];
  (
    [1, 2, 3, 4][1..3] ?? empty,
    b"abcd"[1..3] ?? b"",
    "Allen"[1..4] ?? "",
    [1, 2][1..9] ?? empty
  )
}
"#,
        ),
        Value::Tuple(
            vec![
                Value::List(vec![Value::Int(2), Value::Int(3)].into()),
                Value::Bytes(vec![b'b', b'c'].into()),
                Value::String("lle".to_owned().into()),
                Value::List(Vec::new().into()),
            ]
            .into()
        )
    );
}

#[test]
fn declared_result_types_flow_into_coalescing_and_conditional_literals() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns List<Int> {
  let values = [1, 2, 3];
  let list_slice: List<Int> = values[0..1] ?? [];
  list_slice
}
"#,
        ),
        Value::List(vec![Value::Int(1)].into())
    );

    let source = r#"
fn no_map() returns Option<Map<String, Int>> { None }
export fn main() returns Int {
  let mapping: Map<String, Int> = no_map() ?? map {};
  let conditional: List<Int> = if (true) { [] } else { [1] };
  let selected: { values: List<Int> } = match true {
    true => { values: [] },
    false => { values: [1] }
  };
  length(mapping) + length(conditional) + length(selected.values)
}
"#;
    let compilation = compile_source(source).expect("expected aggregate literals compile");
    verify(compilation.module).expect("expected aggregate literal bytecode verifies");
}

#[test]
fn coalescing_empty_literals_remain_ambiguous_without_an_expected_type() {
    for source in [
        r#"
fn no_list() returns Option<List<Int>> { None }
export fn main() returns Int {
  let values = no_list() ?? [];
  length(values)
}
"#,
        r#"
fn no_map() returns Option<Map<String, Int>> { None }
export fn main() returns Int {
  let values = no_map() ?? map {};
  length(values)
}
"#,
    ] {
        let diagnostics = compile_source(source).expect_err("untyped empty fallback is ambiguous");
        assert_eq!(diagnostics[0].code, "E3010", "{source}\n{diagnostics:?}");
        assert!(
            diagnostics[0].message.contains("requires an expected"),
            "{source}\n{diagnostics:?}"
        );
    }
}

#[test]
fn every_sequence_operation_executes_with_lazy_adapter_order() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns (List<Int>, Option<Int>, Bool, Bool, Int) {
  let materialized = seq.to_list(
    seq.take(
      seq.filter(
        seq.map(seq.from_list([1, 2, 3, 4]), fn(value: Int) returns Int { value + 1 }),
        fn(value: Int) returns Bool { value % 2 == 0 }
      ),
      2
    )
  );
  let found = seq.find(
    seq.from_list([1, 2, 3, 4]),
    fn(value: Int) returns Bool { value == 3 }
  );
  let has_any = seq.any(
    seq.from_list([1, 2, 3, 4]),
    fn(value: Int) returns Bool { value > 3 }
  );
  let all = seq.all(
    seq.from_list([1, 2, 3, 4]),
    fn(value: Int) returns Bool { value > 0 }
  );
  let folded = seq.fold(
    seq.from_list([1, 2, 3, 4]),
    0,
    fn(total: Int, value: Int) returns Int { total + value }
  );
  (materialized, found, has_any, all, folded)
}
"#,
        ),
        Value::Tuple(
            vec![
                Value::List(vec![Value::Int(2), Value::Int(4)].into()),
                Value::Enum(Rc::new(EnumValue {
                    identity: EnumIdentity::Option,
                    type_name: "Option".into(),
                    variant: 1,
                    variant_name: "Some".into(),
                    payload: EnumPayload::Tuple(vec![Value::Int(3)].into()),
                })),
                Value::Bool(true),
                Value::Bool(true),
                Value::Int(10),
            ]
            .into()
        )
    );
}

#[test]
fn consumed_sequence_pipeline_does_not_poison_a_later_range_backedge() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns Int {
  let materialized = seq.to_list(
    seq.take(
      seq.map(
        seq.from_list([1, 2, 3]),
        fn(value: Int) returns Int { value + 1 }
      ),
      2
    )
  );
  let found = seq.find(
    seq.from_list([1, 2, 3]),
    fn(value: Int) returns Bool { value == 2 }
  ) ?? 0;
  mut total = length(materialized) + found;
  let inclusive: Range<Int> = 1..=3;
  for value in inclusive {
    total += value;
  }
  total
}
"#,
        ),
        Value::Int(10)
    );
}

#[test]
fn sequence_adapters_are_lazy_and_terminals_short_circuit() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns (Int, List<Int>, Option<Int>, Bool, Bool) {
  let unused = seq.map(
    seq.from_list([1]),
    fn(value: Int) returns Int { fail("unused map callback ran") }
  );
  let negative_take = seq.to_list(
    seq.take(
      seq.map(
        seq.from_list([1]),
        fn(value: Int) returns Int { fail("negative take callback ran") }
      ),
      -1
    )
  );
  let found = seq.find(
    seq.from_list([1, 2]),
    fn(value: Int) returns Bool {
      if (value == 1) { true } else { fail("find did not short circuit") }
    }
  );
  let has_match = seq.any(
    seq.from_list([1, 2]),
    fn(value: Int) returns Bool {
      if (value == 1) { true } else { fail("any did not short circuit") }
    }
  );
  let all_match = seq.all(
    seq.from_list([0, 1]),
    fn(value: Int) returns Bool {
      if (value == 0) { false } else { fail("all did not short circuit") }
    }
  );
  (42, negative_take, found, has_match, all_match)
}
"#,
        ),
        Value::Tuple(
            vec![
                Value::Int(42),
                Value::List(Vec::new().into()),
                Value::Enum(Rc::new(EnumValue {
                    identity: EnumIdentity::Option,
                    type_name: "Option".into(),
                    variant: 1,
                    variant_name: "Some".into(),
                    payload: EnumPayload::Tuple(vec![Value::Int(1)].into()),
                })),
                Value::Bool(true),
                Value::Bool(false),
            ]
            .into()
        )
    );
}

#[test]
fn sequence_calls_support_labels_extensions_and_pipelines() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns (List<Int>, List<Int>) {
  let labeled = seq.to_list(values: seq.map(
    callback: fn(value: Int) returns Int { value + 1 },
    values: seq.from_list(values: [1, 2])
  ));
  let piped = [1, 2]
    |> seq.from_list()
    |> seq.map(fn(value: Int) returns Int { value + 1 })
    |> seq.to_list();
  (labeled, piped)
}
"#,
        ),
        Value::Tuple(
            vec![
                Value::List(vec![Value::Int(2), Value::Int(3)].into()),
                Value::List(vec![Value::Int(2), Value::Int(3)].into()),
            ]
            .into()
        )
    );

    assert_eq!(
        execute_source(
            r#"
export fn main() returns List<Int> {
  seq.from_list([1, 2])
    .map(fn(value: Int) returns Int { value + 1 })
    .to_list()
}
"#,
        ),
        Value::List(vec![Value::Int(2), Value::Int(3)].into())
    );
}

#[test]
fn sequences_may_be_dropped_but_cannot_be_consumed_twice() {
    compile_source(r#"export fn main() returns Int { let values = seq.from_list([1]); 0 }"#)
        .expect("an unconsumed Sequence has bounded drop cleanup");
    let source = r#"export fn main() returns List<Int> { let values = seq.from_list([1]); let first = seq.to_list(values); seq.to_list(values) }"#;
    assert!(
        compile_source(source).is_err(),
        "a Sequence cannot be consumed twice"
    );
}

#[test]
fn sequence_fold_transfers_affine_initial_accumulator() {
    assert_eq!(
        execute_source(
            r#"
export fn main() returns List<Int> {
  let seed = seq.from_list([0]);
  let empty: List<Int> = [];
  let folded = seq.fold(
    seq.from_list(empty),
    seed,
    fn(acc: Sequence<Int>, value: Int) returns Sequence<Int> { acc }
  );
  seq.to_list(folded)
}
"#,
        ),
        Value::List(vec![Value::Int(0)].into())
    );

    let source = r#"
export fn main() returns List<Int> {
  let seed = seq.from_list([0]);
  let empty: List<Int> = [];
  let folded = seq.fold(
    seq.from_list(empty),
    seed,
    fn(acc: Sequence<Int>, value: Int) returns Sequence<Int> { acc }
  );
  seq.to_list(seed)
}
"#;
    assert!(
        compile_source(source).is_err(),
        "fold must move an affine initial accumulator"
    );
}

#[test]
fn ranges_and_sequences_are_rejected_at_public_boundaries() {
    for source in [
        r#"export fn main(value: Range<Int>) returns Int { 0 }"#,
        r#"export fn main() returns Range<Int> { 1..3 }"#,
        r#"export fn main(values: Sequence<Int>) returns Int { 0 }"#,
        r#"export fn main() returns Sequence<Int> { seq.from_list([1]) }"#,
    ] {
        let bundle = PackageSourceBundle {
            root: "main.allen".to_owned(),
            sources: BTreeMap::from([("main.allen".to_owned(), source.to_owned())]),
            import_targets: BTreeMap::new(),
            entry_modules: vec!["main.allen".to_owned()],
            entry_points: vec![PackageEntryPoint {
                module: "main.allen".to_owned(),
                function: "main".to_owned(),
            }],
        };
        let rejected = match compile_package_bundle(&bundle) {
            Ok(compilation) => assemble_loose_compilation("main.allen", compilation).is_err(),
            Err(_) => true,
        };
        assert!(rejected, "non-serializable boundary must fail: {source}");
    }
}

#[test]
fn invalid_range_slice_and_sequence_shapes_are_rejected() {
    for source in [
        r#"export fn main() returns Range<Int> { 1.0..2.0 }"#,
        r#"export fn main() returns List<Int> { [1, 2][0..=1] ?? [] }"#,
        r#"export fn main() returns List<Int> { seq.to_list(seq.from_list([1]), 2) }"#,
        r#"export fn main() returns List<Int> { seq.to_list(seq.map(seq.from_list([1]), fn(value: Int) returns Bool { true })) }"#,
        r#"export fn main() returns List<Int> { seq.to_list(seq.map(seq.from_list([1]), fn(value: Int) returns Int effects [user.ask] { value })) }"#,
    ] {
        assert!(
            compile_source(source).is_err(),
            "invalid L3 value shape must fail: {source}"
        );
    }
}
