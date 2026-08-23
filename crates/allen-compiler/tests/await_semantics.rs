use allen_bytecode::{EffectOperation, Instruction, RecordField, ValueType, verify};
use allen_compiler::{
    CompilerToolBinding, MirCleanupKind, MirOperation, MirTerminator, PackageEntryPoint,
    PackageSourceBundle, compile_package_bundle_with_tools, compile_source,
};
use allen_vm::Value;
use std::collections::BTreeMap;

#[test]
fn await_expressions_and_nested_blocks_use_innermost_task_scope() {
    let source = concat!(
        "async fn number(value: Int) returns Int { value }\n",
        "export async fn main() returns Int effects [task.spawn] {\n",
        "  let outside = await number(1);\n",
        "  await {\n",
        "    let outer = spawn number(40);\n",
        "    let inner_value = await {\n",
        "      let inner = spawn number(2);\n",
        "      await number(1)\n",
        "    };\n",
        "    let outer_value = await outer;\n",
        "    outside + inner_value + outer_value\n",
        "  }\n",
        "}\n",
    );
    let compilation = compile_source(source).expect("nested await blocks compile");
    let main = &compilation.module.functions[compilation.module.entry as usize];
    let entered = main
        .code
        .iter()
        .filter_map(|instruction| match instruction {
            Instruction::TaskScopeEnter { scope } => Some(*scope),
            _ => None,
        })
        .collect::<Vec<_>>();
    let spawned = main
        .code
        .iter()
        .filter_map(|instruction| match instruction {
            Instruction::Spawn { scope, .. } => Some(*scope),
            _ => None,
        })
        .collect::<Vec<_>>();
    let exited = main
        .code
        .iter()
        .filter_map(|instruction| match instruction {
            Instruction::TaskScopeExit { scope } => Some(*scope),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(entered, [1, 2]);
    assert_eq!(spawned, [1, 2]);
    assert_eq!(exited, [2, 1]);

    let verified = verify(compilation.module).expect("nested await blocks verify");
    assert_eq!(
        allen_vm::execute(&verified).expect("nested await blocks execute"),
        Value::Int(42)
    );
}

#[test]
fn direct_provider_futures_are_lazy_but_must_be_consumed() {
    let source = concat!(
        "export async fn main() returns Int effects [agent.message] {\n",
        "  let forgotten = agent.message(\"hello\");\n",
        "  1\n",
        "}\n",
    );
    let diagnostics = compile_source(source).expect_err("a direct provider future is must-consume");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].code, "E3011");
    assert!(
        diagnostics[0].message.contains("live affine obligation"),
        "{diagnostics:?}"
    );

    let valid = concat!(
        "export async fn main() returns Result<Void, AgentError> effects [agent.message] {\n",
        "  let pending = agent.message(\"hello\");\n",
        "  await pending\n",
        "}\n",
    );
    let compilation = compile_source(valid).expect("explicitly awaited provider future compiles");
    verify(compilation.module.clone()).expect("direct provider future bytecode verifies");
    let main = &compilation.module.functions[compilation.module.entry as usize];
    let (effect_index, future) = main
        .code
        .iter()
        .enumerate()
        .find_map(|(index, instruction)| match instruction {
            Instruction::EffectCall {
                destination,
                operation: EffectOperation::AgentMessage,
                ..
            } => Some((index, *destination)),
            _ => None,
        })
        .expect("agent.message future construction");
    assert!(matches!(
        main.registers[future as usize],
        ValueType::Future(_)
    ));
    let await_index = main
        .code
        .iter()
        .enumerate()
        .find_map(|(index, instruction)| match instruction {
            Instruction::Await { source, .. } if *source == future => Some(index),
            _ => None,
        })
        .expect("the provider future is awaited");
    assert!(effect_index < await_index);

    let spawned = concat!(
        "export async fn main() returns Result<Void, AgentError> ",
        "effects [agent.message, task.spawn] {\n",
        "  await {\n",
        "    let pending = agent.message(\"hello\");\n",
        "    let task = spawn pending;\n",
        "    await task\n",
        "  }\n",
        "}\n",
    );
    let compilation = compile_source(spawned).expect("spawned provider future compiles");
    verify(compilation.module.clone()).expect("spawned provider future bytecode verifies");
    let main = &compilation.module.functions[compilation.module.entry as usize];
    let (effect_index, future) = main
        .code
        .iter()
        .enumerate()
        .find_map(|(index, instruction)| match instruction {
            Instruction::EffectCall {
                destination,
                operation: EffectOperation::AgentMessage,
                ..
            } => Some((index, *destination)),
            _ => None,
        })
        .expect("agent.message future construction");
    let spawn_index = main
        .code
        .iter()
        .enumerate()
        .find_map(|(index, instruction)| match instruction {
            Instruction::Spawn { future: source, .. } if *source == future => Some(index),
            _ => None,
        })
        .expect("the provider future is spawned");
    assert!(effect_index < spawn_index);
}

#[test]
fn try_error_joins_scopes_while_terminal_await_edges_cancel_them() {
    let source = concat!(
        "async fn number() returns Int { 1 }\n",
        "fn fallible() returns Result<Int, Bool> { Err(false) }\n",
        "export async fn main() returns Result<Int, Bool> effects [task.spawn] {\n",
        "  await {\n",
        "    let outer = spawn number();\n",
        "    await {\n",
        "      let inner = spawn number();\n",
        "      let value = fallible()?;\n",
        "      let inner_number = await inner;\n",
        "      let outer_number = await outer;\n",
        "      Ok(value + inner_number + outer_number)\n",
        "    }\n",
        "  }\n",
        "}\n",
    );
    let compilation = compile_source(source).expect("try scope compiles");
    let main = compilation
        .mir
        .functions
        .iter()
        .find(|function| function.name == "main")
        .expect("main MIR");
    main.validate_cfg().expect("main MIR CFG is valid");

    let try_error = main
        .blocks
        .iter()
        .find_map(|block| match block.terminator {
            MirTerminator::TryResult { error, .. } => Some(error),
            _ => None,
        })
        .expect("try error edge");
    let try_cleanup = main.blocks[try_error as usize]
        .operations
        .iter()
        .filter_map(|operation| match operation {
            MirOperation::TaskScopeCleanup { scope, kind } => Some((*scope, *kind)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        try_cleanup,
        [
            (2, MirCleanupKind::NormalJoin),
            (1, MirCleanupKind::NormalJoin),
        ]
    );

    assert_eq!(main.suspensions.len(), 2);
    for suspension in &main.suspensions {
        for (target, expected) in [
            (
                suspension.exceptional_cancel,
                MirCleanupKind::ExceptionalCancel,
            ),
            (suspension.timeout_cancel, MirCleanupKind::TimeoutCancel),
            (suspension.external_cancel, MirCleanupKind::ExternalCancel),
            (suspension.permanent_stop, MirCleanupKind::PermanentStop),
        ] {
            let cleanup = main.blocks[target as usize]
                .operations
                .iter()
                .filter_map(|operation| match operation {
                    MirOperation::TaskScopeCleanup { scope, kind } => Some((*scope, *kind)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(cleanup, [(2, expected), (1, expected)]);
        }
    }
}

#[test]
fn generated_tool_call_futures_are_must_consume() {
    let root = "pkg://await-semantics@0.1.0/src/main.allen";
    let bundle = PackageSourceBundle {
        root: root.to_owned(),
        sources: BTreeMap::from([(
            root.to_owned(),
            concat!(
                "export async fn main() returns Int effects [tool.example.echo@1] {\n",
                "  let forgotten = tools.example.echo.call({ value: \"hello\" });\n",
                "  1\n",
                "}\n",
            )
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
    let diagnostics = compile_package_bundle_with_tools(&bundle, &[binding])
        .expect_err("a generated tool-call future is must-consume");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].code, "E3011");
    assert!(
        diagnostics[0].message.contains("live affine obligation"),
        "{diagnostics:?}"
    );
}
