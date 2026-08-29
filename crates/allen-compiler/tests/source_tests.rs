use allen_compiler::{PackageSourceBundle, compile_source_test, discover_source_tests};
use std::collections::BTreeMap;

fn bundle(source: &str) -> PackageSourceBundle {
    PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([("main.allen".to_owned(), source.to_owned())]),
        import_targets: BTreeMap::new(),
        entry_points: Vec::new(),
        entry_modules: Vec::new(),
    }
}

struct Never;

impl allen_vm::CancellationSource for Never {
    fn is_cancelled(&mut self) -> bool {
        false
    }
}

struct Ignore;

impl allen_vm::CheckpointObserver for Ignore {
    fn checkpoint(&mut self, _: allen_vm::Checkpoint) {}
}

struct Reject;

impl allen_vm::EffectProvider for Reject {
    fn workspace(&mut self) -> Result<allen_vm::WorkspaceValue, allen_vm::VmError> {
        Err(allen_vm::VmError::CapabilityMissing)
    }

    fn call(
        &mut self,
        _: allen_bytecode::EffectOperation,
        _: &[allen_vm::Value],
    ) -> Result<allen_vm::Value, allen_vm::VmError> {
        Err(allen_vm::VmError::CapabilityMissing)
    }
}

#[test]
fn discovers_and_isolates_source_tests() {
    let source = r#"
fn helper() returns Int { 41 }
test "pass" { let value = helper(); () }
test "fail" { fail("intentional") }
"#;
    let bundle = bundle(source);
    let tests = discover_source_tests(&bundle).unwrap();
    assert_eq!(
        tests
            .iter()
            .map(|test| test.name.as_str())
            .collect::<Vec<_>>(),
        ["pass", "fail"]
    );
    let compiled = compile_source_test(&bundle, "main.allen", "fail").unwrap();
    let verified = allen_bytecode::verify(compiled.compilation.module).unwrap();
    let error = allen_vm::execute(&verified).unwrap_err();
    assert_eq!(error.code(), "program.failed");

    let compiled = compile_source_test(&bundle, "main.allen", "fail").unwrap();
    let artifact = allen_compiler::assemble_source_test(compiled.compilation).unwrap();
    let bytes = allen_bytecode::encode(&artifact).unwrap();
    let verified =
        allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
            .unwrap();
    let entry = verified
        .entries()
        .iter()
        .find(|entry| entry.name == "test")
        .unwrap();
    let mut clock = allen_vm::SystemMonotonicClock::new();
    let error = allen_vm::execute_entry_with_runtime_context(
        verified.verified_module(),
        None,
        entry.function,
        &[],
        allen_vm::ExecutionLimits::default(),
        &mut clock,
        &mut Ignore,
        &mut Never,
        &mut Reject,
    )
    .unwrap_err();
    assert_eq!(error.code(), "program.failed");
}

#[test]
fn production_compilation_is_byte_identical_with_tests_present() {
    let plain = "export fn main() returns Int { 7 }";
    let with_test = "export fn main() returns Int { 7 } test \"ignored\" { () }";
    let plain = allen_compiler::compile_source(plain).unwrap();
    let with_test = allen_compiler::compile_source(with_test).unwrap();
    assert_eq!(plain.module, with_test.module);
    let plain = allen_compiler::assemble_loose_compilation("main.allen", plain).unwrap();
    let with_test = allen_compiler::assemble_loose_compilation("main.allen", with_test).unwrap();
    assert_eq!(
        allen_bytecode::encode(&plain.artifact).unwrap(),
        allen_bytecode::encode(&with_test.artifact).unwrap()
    );
}

#[test]
fn unreferenced_modules_and_equal_display_names_are_module_qualified() {
    let bundle = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            (
                "main.allen".to_owned(),
                "export fn main() returns Void { () } test \"same\" { () }".to_owned(),
            ),
            (
                "checks/extra.allen".to_owned(),
                "fn helper() returns Int { 1 } test \"same\" { let value = helper(); () }"
                    .to_owned(),
            ),
        ]),
        import_targets: BTreeMap::new(),
        entry_points: Vec::new(),
        entry_modules: Vec::new(),
    };
    let tests = discover_source_tests(&bundle).unwrap();
    assert_eq!(
        tests
            .iter()
            .map(|test| (test.module.as_str(), test.name.as_str()))
            .collect::<Vec<_>>(),
        [("checks/extra.allen", "same"), ("main.allen", "same")]
    );
    compile_source_test(&bundle, "checks/extra.allen", "same").unwrap();
}

#[test]
fn duplicate_test_names_are_rejected_within_one_module() {
    let error = discover_source_tests(&bundle(
        "test \"duplicate\" { () } test \"duplicate\" { () }",
    ))
    .unwrap_err();
    assert!(error[0].message.contains("duplicate test name"));
}
