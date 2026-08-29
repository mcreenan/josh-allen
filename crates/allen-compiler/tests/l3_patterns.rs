#![allow(clippy::needless_raw_string_hashes)]

use allen_bytecode::{Instruction, verify};
use allen_compiler::compile_source;
use allen_vm::{Value, execute};

fn execute_source(source: &str) -> Value {
    let compilation = compile_source(source).expect("L3 pattern source compiles");
    execute(&verify(compilation.module).expect("L3 pattern bytecode verifies"))
        .expect("L3 pattern source executes")
}

fn compile_error(source: &str) -> allen_compiler::Diagnostic {
    compile_source(source)
        .expect_err("source must be rejected")
        .into_iter()
        .next()
        .expect("compiler returns one diagnostic")
}

#[test]
fn int_ranges_cover_negative_half_open_and_inclusive_boundaries() {
    assert_eq!(
        execute_source(
            r#"
fn classify(value: Int) returns Int {
  match value {
    -5..-1 => 1,
    -1..=1 => 2,
    _ => 3,
  }
}
export fn main() returns (Int, Int, Int, Int) {
  (classify(-5), classify(-1), classify(1), classify(2))
}
"#,
        ),
        Value::Tuple(vec![Value::Int(1), Value::Int(2), Value::Int(2), Value::Int(3)].into())
    );
}

#[test]
fn string_and_bytes_ranges_use_language_lexicographic_ordering() {
    assert_eq!(
        execute_source(
            r#"
fn text(value: String) returns Bool {
  match value { "a"..="é" => true, _ => false }
}
fn bytes(value: Bytes) returns Bool {
  match value { b"\x7f"..=b"\xff" => true, _ => false }
}
export fn main() returns (Bool, Bool, Bool, Bool) {
  (text("z"), text("ê"), bytes(b"\x80"), bytes(b"\x01"))
}
"#,
        ),
        Value::Tuple(
            vec![
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(false),
            ]
            .into()
        )
    );
}

#[test]
fn or_patterns_share_one_body_and_preserve_left_to_right_choice() {
    assert_eq!(
        execute_source(
            r#"
enum Number { Small(Int), Large(Int) }
fn unwrap(value: Number) returns Int {
  match value {
    Number.Small(item) | Number.Large(item) => item,
  }
}
export fn main() returns (Int, Int) {
  (unwrap(Number.Small(3)), unwrap(Number.Large(9)))
}
"#,
        ),
        Value::Tuple(vec![Value::Int(3), Value::Int(9)].into())
    );
}

#[test]
fn nested_or_patterns_fall_through_inside_payloads() {
    assert_eq!(
        execute_source(
            r#"
fn classify(value: Option<Int>) returns Int {
  match value {
    Some(1..=2 | 4..=5) => 1,
    Some(_) => 2,
    None => 3,
  }
}
export fn main() returns (Int, Int, Int) {
  (classify(Some(2)), classify(Some(3)), classify(None))
}
"#,
        ),
        Value::Tuple(vec![Value::Int(1), Value::Int(2), Value::Int(3)].into())
    );
}

#[test]
fn ranges_reject_float_mixed_and_empty_endpoints() {
    for source in [
        r#"export fn main() returns Int { match 1.0 { 0.0..=2.0 => 1, _ => 0 } }"#,
        r#"export fn main() returns Int { match 1 { 0..="z" => 1, _ => 0 } }"#,
        r#"export fn main() returns Int { match 1 { 2..2 => 1, _ => 0 } }"#,
        r#"export fn main() returns Int { match 1 { 3..=2 => 1, _ => 0 } }"#,
    ] {
        let diagnostic = compile_error(source);
        assert_eq!(diagnostic.code, "E3007");
    }
}

#[test]
fn or_alternatives_require_identical_bindings() {
    let diagnostic = compile_error(
        r#"
enum Number { Small(Int), Large(Int) }
export fn main() returns Int {
  match Number.Small(1) {
    Number.Small(left) | Number.Large(right) => left,
  }
}
"#,
    );
    assert_eq!(diagnostic.code, "E3007");
    assert!(diagnostic.message.contains("does not bind 'left'"));
}

#[test]
fn fully_covered_alternatives_are_unreachable_but_partial_overlap_is_allowed() {
    let unreachable = compile_error(
        r#"
export fn main() returns Int {
  match 3 { 0..=10 | 2..=4 => 1, _ => 0 }
}
"#,
    );
    assert_eq!(unreachable.code, "E2016");

    assert_eq!(
        execute_source(
            r#"
export fn main() returns Int {
  match 7 { 0..=5 => 1, 3..=8 => 2, _ => 3 }
}
"#,
        ),
        Value::Int(2)
    );
}

#[test]
fn inclusive_predecessors_cover_adjacent_half_open_range_ends() {
    for source in [
        r#"export fn main() returns Int { match 1 { 0..=1 => 1, 0..2 => 2, _ => 3 } }"#,
        r#"export fn main() returns Int { match "a" { "a"..="a" => 1, "a".."a\0" => 2, _ => 3 } }"#,
        r#"export fn main() returns Int { match b"a" { b"a"..=b"a" => 1, b"a"..b"a\x00" => 2, _ => 3 } }"#,
    ] {
        let diagnostic = compile_error(source);
        assert_eq!(diagnostic.code, "E2016", "{source}\n{diagnostic:?}");
    }
}

#[test]
fn nested_or_payload_coverage_makes_later_patterns_unreachable() {
    for source in [
        r#"export fn main() returns Int { match Some(2) { Some(0..=2 | 4..=5) => 1, Some(2..=2) => 2, _ => 3 } }"#,
        r#"fn value() returns Result<Int, Bool> { Ok(2) } export fn main() returns Int { match value() { Ok(0..=2 | 4..=5) => 1, Ok(2..=2) => 2, _ => 3 } }"#,
        r#"enum Boxed { Value(Int) } export fn main() returns Int { match Boxed.Value(2) { Boxed.Value(0..=2 | 4..=5) => 1, Boxed.Value(2..=2) => 2, _ => 3 } }"#,
        r#"record Boxed { value: Int } fn value() returns Boxed { Boxed { value: 2 } } export fn main() returns Int { match value() { Boxed { value: 0..=2 | 4..=5 } => 1, Boxed { value: 2..=2 } => 2, _ => 3 } }"#,
    ] {
        let diagnostic = compile_error(source);
        assert_eq!(diagnostic.code, "E2016", "{source}\n{diagnostic:?}");
    }
}

#[test]
fn non_exhaustive_range_matches_require_a_catch_all() {
    let diagnostic = compile_error(r#"export fn main() returns Int { match 3 { 0..=10 => 1 } }"#);
    assert_eq!(diagnostic.code, "E2015");
}

#[test]
fn complete_int_ranges_are_exhaustive_at_both_machine_boundaries() {
    assert_eq!(
        execute_source(
            r#"
fn classify(value: Option<Int>) returns Int {
  match value {
    Some(-9223372036854775808..=9223372036854775807) => 1,
    None => 0,
  }
}
export fn main() returns (Int, Int) {
  (classify(Some(-9223372036854775808)), classify(None))
}
"#,
        ),
        Value::Tuple(vec![Value::Int(1), Value::Int(0)].into())
    );
}

#[test]
fn nested_finite_or_ranges_are_exhaustive_in_all_container_patterns() {
    for source in [
        r#"
fn classify(value: Option<Int>) returns Int {
  match value {
    Some(-9223372036854775808..=-1 | 0..=9223372036854775807) => 1,
    None => 0,
  }
}
export fn main() returns Int { classify(Some(1)) }
"#,
        r#"
fn classify(value: Result<Int, Bool>) returns Int {
  match value {
    Ok(-9223372036854775808..=-1 | 0..=9223372036854775807) => 1,
    Err(false) | Err(true) => 0,
  }
}
export fn main() returns Int { classify(Ok(1)) }
"#,
        r#"
enum Boxed { Value(Int), Empty }
fn classify(value: Boxed) returns Int {
  match value {
    Boxed.Value(-9223372036854775808..=-1 | 0..=9223372036854775807) => 1,
    Boxed.Empty => 0,
  }
}
export fn main() returns Int { classify(Boxed.Value(1)) }
"#,
        r#"
record Boxed { value: Int }
fn classify(value: Boxed) returns Int {
  match value {
    Boxed { value: -9223372036854775808..=-1 | 0..=9223372036854775807 } => 1,
  }
}
export fn main() returns Int { classify(Boxed { value: 1 }) }
"#,
    ] {
        let compilation = compile_source(source).expect("nested finite coverage is exhaustive");
        verify(compilation.module).expect("nested finite coverage verifies");
    }
}

#[test]
fn advanced_match_scrutinee_call_is_emitted_once() {
    let compilation = compile_source(
        r#"
fn source() returns Int { 3 }
export fn main() returns Int {
  match source() { 0..=5 | 8..=9 => 1, _ => 0 }
}
"#,
    )
    .expect("range match compiles");
    let source_id = u32::try_from(
        compilation
            .module
            .functions
            .iter()
            .position(|function| function.name.ends_with("::source"))
            .expect("source function exists"),
    )
    .expect("function ID fits");
    let main = compilation
        .module
        .functions
        .iter()
        .find(|function| function.name.ends_with("::main"))
        .expect("main function exists");
    assert_eq!(
        main.code
            .iter()
            .filter(|instruction| {
                matches!(instruction, Instruction::DirectCall { function, .. } if *function == source_id)
            })
            .count(),
        1
    );
}

#[test]
fn affine_or_bindings_move_one_exact_payload() {
    compile_source(
        r#"
enum Pending { First(Future<Int>), Second(Future<Int>) }
async fn choose(value: Pending) returns Int {
  match value {
    Pending.First(future) | Pending.Second(future) => await future,
  }
}
export fn main() returns Int { 0 }
"#,
    )
    .expect("OR alternatives move one exact affine payload");

    let duplicate = compile_error(
        r#"
enum Pending { Both(Future<Int>, Future<Int>) }
fn choose(value: Pending) returns Future<Int> {
  match value { Pending.Both(future, future) => future }
}
export fn main() returns Int { 0 }
"#,
    );
    assert_eq!(duplicate.code, "E3005");
}

#[test]
fn wildcard_inside_nested_or_makes_later_alternatives_unreachable() {
    let diagnostic = compile_error(
        r#"
export fn main() returns Int {
  match Some(2) {
    Some(_ | 1..=3) => 1,
    None => 0,
  }
}
"#,
    );
    assert_eq!(diagnostic.code, "E2016");
}
