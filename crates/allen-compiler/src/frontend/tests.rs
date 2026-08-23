    #![allow(clippy::needless_raw_string_hashes)]

    use super::*;
    use allen_bytecode::{FsOperation, verify};

    struct TutorialObserver;

    impl allen_vm::CheckpointObserver for TutorialObserver {
        fn checkpoint(&mut self, _checkpoint: allen_vm::Checkpoint) {}
    }

    struct TutorialCancellation;

    impl allen_vm::CancellationSource for TutorialCancellation {
        fn is_cancelled(&mut self) -> bool {
            false
        }
    }

    struct TutorialEffects;

    impl allen_vm::EffectProvider for TutorialEffects {
        fn workspace(&mut self) -> Result<allen_vm::WorkspaceValue, allen_vm::VmError> {
            Err(allen_vm::VmError::CapabilityMissing)
        }

        fn call(
            &mut self,
            _operation: allen_bytecode::EffectOperation,
            _arguments: &[allen_vm::Value],
        ) -> Result<allen_vm::Value, allen_vm::VmError> {
            Err(allen_vm::VmError::CapabilityMissing)
        }
    }

    fn nominal_sources() -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "main.allen".to_owned(),
                r#"
import { Token as LeftToken, make as make_left } from "./left.allen";
import { Token as RightToken, make as make_right } from "./right.allen";

export fn main() returns (LeftToken, RightToken) {
  (make_left(), make_right())
}
"#
                .to_owned(),
            ),
            (
                "left.allen".to_owned(),
                r"
export enum Token { Only }
export fn make() returns Token { Token.Only }
"
                .to_owned(),
            ),
            (
                "right.allen".to_owned(),
                r"
export enum Token { Only }
export fn make() returns Token { Token.Only }
"
                .to_owned(),
            ),
        ])
    }

    #[test]
    fn comments_are_whitespace_but_literal_delimiters_remain_data() {
        let plain = r#"fn answer() returns Int { 42 }
export fn main() returns (String, Bytes, Int) {
  ("// /* */", b"// /* */", answer())
}"#;
        let commented = r#"//! ordinary line comment
fn answer() returns Int { 42 } // after a token
// between declarations
/*! ordinary doc-looking block comment */
export /** ordinary block comment /* nested */ */ fn main() returns (String, Bytes, Int) {
  // inside a function body
  /* quotes " and backslashes \\ and braces { } are comment text
     across multiple lines */
  ("// /* */", b"// /* */", /* between expression tokens */ answer())
} // end-of-file comment"#;
        let plain = compile_source(plain).expect("plain source compiles");
        let commented = compile_source(commented).expect("commented source compiles");
        assert_eq!(commented.module, plain.module);
        assert_eq!(commented.effect_report, plain.effect_report);
        assert_eq!(commented.exported_functions, plain.exported_functions);
        assert_ne!(commented.debug, plain.debug);
        verify(commented.module).expect("commented module verifies");
    }

    #[test]
    fn comments_multibyte_comment_text_keeps_diagnostic_byte_offsets_exact() {
        let source = "/* 🦀 λ */ export fn main() returns Int { missing }";
        let diagnostics = compile_source(source).expect_err("unknown name must fail");
        assert_eq!(diagnostics.len(), 1);
        let start = source.find("missing").unwrap();
        assert_eq!(
            diagnostics[0].span,
            Span {
                start,
                end: start + 7
            }
        );
        assert_eq!(&source[start..start + 7], "missing");
    }

    #[test]
    fn comments_inline_manifest_accepts_comments_without_changing_its_contract() {
        let plain = r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
}
export fn main() returns Int { 42 }
"#;
        let commented = r#"/// ordinary comment before an inline manifest
manifest /* between tokens */ {
  language: /* before a value */ "0.1"
  entry: main // after a value
  capabilities: [/* empty */]
}
export fn main() returns Int { 42 }
"#;
        let (plain_manifest, plain_compilation) =
            compile_inline_manifest_source(plain).expect("plain inline source compiles");
        let (commented_manifest, commented_compilation) =
            compile_inline_manifest_source(commented).expect("commented inline source compiles");
        assert_eq!(commented_manifest, plain_manifest);
        assert_eq!(commented_compilation.module, plain_compilation.module);
        assert_eq!(
            commented_compilation.exported_functions,
            plain_compilation.exported_functions
        );
    }

    #[test]
    fn async_calls_are_lazy_and_await_removes_one_layer() {
        let source = r#"
async fn number() returns Int { 7 }
async fn start() returns Task<Int> effects [task.spawn] { spawn number() }
export async fn main() returns Int effects [task.spawn] {
  let task = await start();
  await task
}
"#;
        let compilation = compile_source(source).expect("async source compiles");
        verify(compilation.module.clone()).expect("async module verifies");
        assert_eq!(compilation.module.async_functions, vec![0, 1, 2]);
        assert!(compilation.module.functions.iter().any(|function| {
            function
                .code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::AsyncCall { .. }))
        }));
        assert!(compilation.module.functions.iter().any(|function| {
            function
                .code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::Await { .. }))
        }));
        assert!(
            compilation
                .hir
                .types
                .contains(&ValueType::Future(Box::new(ValueType::Task(Box::new(
                    ValueType::Int
                )))))
        );
        assert!(
            compilation
                .mir
                .functions
                .iter()
                .any(|function| !function.suspensions.is_empty())
        );
    }

    #[test]
    fn await_blocks_emit_scopes_and_cleanup_edges() {
        let source = r#"
async fn number() returns Int { 7 }
export async fn main() returns Int effects [task.spawn] {
  await {
    let task = spawn number();
    return await task;
  }
}
"#;
        let compilation = compile_source(source).expect("await block compiles");
        verify(compilation.module.clone()).expect("await-block module verifies");
        let main = &compilation.module.functions[compilation.module.entry as usize];
        assert!(matches!(
            main.code.first(),
            Some(Instruction::TaskScopeEnter { scope: 1 })
        ));
        assert!(
            main.code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::Spawn { scope: 1, .. }))
        );
        assert!(
            main.code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::TaskScopeExit { scope: 1 }))
        );
        let exit = main
            .code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::TaskScopeExit { .. }))
            .expect("scope exit");
        let returned = main
            .code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::Return { .. }))
            .expect("function return");
        assert!(exit < returned);
        let mir = compilation
            .mir
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main MIR");
        assert_eq!(mir.task_scopes.len(), 1);
        assert!(
            mir.ownership
                .iter()
                .any(|entry| entry.state == MirOwnershipState::Awaited)
        );
        let suspension = mir.suspensions.first().expect("await suspension");
        assert_ne!(suspension.resume, suspension.exceptional_cancel);
        assert!(mir.blocks.iter().any(|block| {
            matches!(block.terminator, MirTerminator::Suspend { .. })
                && block
                    .operations
                    .iter()
                    .any(|operation| matches!(operation, MirOperation::Await { .. }))
        }));
        assert!(matches!(
            mir.blocks[suspension.resume as usize].terminator,
            MirTerminator::Goto { .. } | MirTerminator::Return { .. }
        ));
        assert!(mir.validate_cfg().is_ok());
    }

    #[test]
    fn task_snapshot_observes_a_task_without_consuming_its_handle() {
        let source = r#"
async fn number() returns Int { 7 }
export async fn main() returns Int effects [debug.inspect, task.spawn] {
  let task = spawn number();
  let snapshot = allen.internal.task_snapshot(task);
  await task
}
"#;
        let compilation = compile_source(source).expect("task snapshot compiles");
        verify(compilation.module.clone()).expect("task snapshot module verifies");
        let main = &compilation.module.functions[compilation.module.entry as usize];
        let (destination, source) = main
            .code
            .iter()
            .find_map(|instruction| match instruction {
                Instruction::TaskSnapshot {
                    destination,
                    source,
                } => Some((*destination, *source)),
                _ => None,
            })
            .expect("task snapshot instruction");
        assert!(main.code.iter().any(|instruction| {
            matches!(instruction, Instruction::Await { source: awaited, .. } if *awaited == source)
        }));
        assert_eq!(main.registers[destination as usize], task_snapshot_type());
        assert!(compilation.mir.functions.iter().any(|function| {
            function.blocks.iter().any(|block| {
                block.operations.iter().any(|operation| {
                    matches!(operation, MirOperation::TaskSnapshot { source: observed, .. } if *observed == u32::from(source))
                })
            })
        }));
    }

    #[test]
    fn task_snapshot_requires_its_local_debug_effect() {
        let missing_effect = r#"
async fn number() returns Int { 7 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  let snapshot = allen.internal.task_snapshot(task);
  await task
}
"#;
        let diagnostics = compile_source(missing_effect).expect_err("effect is required");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E2403");
        assert_eq!(
            diagnostics[0].message,
            "function 'main' requires undeclared effects [debug.inspect, task.spawn]"
        );

        let wrong_arity = r#"
async fn number() returns Int { 7 }
export async fn main() returns Int effects [debug.inspect, task.spawn] {
  let task = spawn number();
  let snapshot = allen.internal.task_snapshot();
  await task
}
"#;
        let diagnostics = compile_source(wrong_arity).expect_err("arity is checked");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "allen.internal.task_snapshot requires exactly one Task<T> argument"
        );

        let wrong_type = r#"
export fn main() returns Int effects [debug.inspect] {
  let snapshot = allen.internal.task_snapshot(7);
  1
}
"#;
        let diagnostics = compile_source(wrong_type).expect_err("type is checked");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "allen.internal.task_snapshot requires Task<T>, found Int"
        );
    }

    #[test]
    fn task_snapshot_is_reported_in_a_private_debug_effect_contract() {
        let source = r#"
async fn number() returns Int { 7 }
async fn inspect() returns Int effects [debug.inspect, task.spawn] {
  let task = spawn number();
  let snapshot = allen.internal.task_snapshot(task);
  await task
}
export async fn main() returns Int effects [debug.inspect, task.spawn] { await inspect() }
"#;
        let compilation = compile_source(source).expect("private effect contract compiles");
        let inspect = compilation
            .effect_report
            .iter()
            .find(|entry| entry.function == "inspect")
            .expect("private inspect effect report");
        assert_eq!(inspect.effects, vec!["debug.inspect", "task.spawn"]);
    }

    #[test]
    fn filesystem_builtins_lower_to_typed_lazy_effect_calls() {
        let source = r#"
export async fn main() returns Result<String, FileError> effects [fs.read] {
  let workspace = fs.workspace();
  await fs.read_text(workspace, "notes.txt")
}
"#;
        let compilation = compile_source(source).expect("filesystem source compiles");
        verify(compilation.module.clone()).expect("filesystem module verifies");
        let main = &compilation.module.functions[compilation.module.entry as usize];
        assert!(
            main.code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::WorkspaceGet { .. }))
        );
        assert!(main.code.iter().any(|instruction| {
            matches!(
                instruction,
                Instruction::EffectCall {
                    operation: FsOperation::ReadText,
                    arguments,
                    ..
                } if arguments.len() == 2
            )
        }));
        assert!(
            compilation
                .module
                .effect_sets
                .contains(&vec!["fs.read".to_owned()])
        );
        assert!(compilation.hir.types.contains(&ValueType::Workspace));
        assert!(
            compilation
                .hir
                .types
                .contains(&ValueType::Future(Box::new(ValueType::Result(
                    Box::new(ValueType::String),
                    Box::new(file_error_type())
                ))))
        );
        assert!(compilation.mir.functions.iter().any(|function| {
            function.blocks.iter().any(|block| {
                block.operations.iter().any(|operation| {
                    matches!(
                        operation,
                        MirOperation::EffectCall {
                            operation: FsOperation::ReadText,
                            arguments,
                            ..
                        } if arguments.len() == 2
                    )
                })
            })
        }));
    }

    #[test]
    fn filesystem_builtins_require_exact_types_and_effects() {
        let missing_effect = r#"
export async fn main() returns Result<String, FileError> {
  let workspace = fs.workspace();
  await fs.read_text(workspace, "notes.txt")
}
"#;
        let diagnostics = compile_source(missing_effect).expect_err("effect is required");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E2403");
        assert_eq!(
            diagnostics[0].message,
            "function 'main' requires undeclared effects [fs.read]"
        );

        let wrong_arity = r#"
export async fn main() returns Result<String, FileError> effects [fs.read] {
  let workspace = fs.workspace();
  await fs.read_text(workspace)
}
"#;
        let diagnostics = compile_source(wrong_arity).expect_err("arity is checked");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "filesystem operation has the wrong argument count"
        );

        let wrong_type = r#"
export async fn main() returns Result<String, FileError> effects [fs.read] {
  await fs.read_text("not-a-workspace", "notes.txt")
}
"#;
        let diagnostics = compile_source(wrong_type).expect_err("workspace is required");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "filesystem operation expected Workspace, found String"
        );
    }

    #[test]
    fn filesystem_builtin_operations_keep_their_result_and_effect_types() {
        let source = r#"
async fn read_bytes() returns Result<Bytes, FileError> effects [fs.read] {
  let workspace = fs.workspace();
  await fs.read_bytes(workspace, "data.bin")
}
async fn write_text() returns Result<Void, FileError> effects [fs.write] {
  let workspace = fs.workspace();
  await fs.write_text(workspace, "notes.txt", "hello")
}
async fn write_bytes(value: Bytes) returns Result<Void, FileError> effects [fs.write] {
  let workspace = fs.workspace();
  await fs.write_bytes(workspace, "data.bin", value)
}
async fn list() returns Result<List<String>, FileError> effects [fs.read] {
  let workspace = fs.workspace();
  await fs.list(workspace, "")
}
async fn search() returns Result<List<SearchMatch>, FileError> effects [fs.read] {
  let workspace = fs.workspace();
  await fs.search(workspace, ".", "needle")
}
export async fn main() returns Result<String, FileError> effects [fs.read] {
  let workspace = fs.workspace();
  await fs.read_text(workspace, "notes.txt")
}
"#;
        let compilation = compile_source(source).expect("filesystem operations compile");
        verify(compilation.module.clone()).expect("filesystem module verifies");
        let operations = compilation
            .module
            .functions
            .iter()
            .flat_map(|function| function.code.iter())
            .filter_map(|instruction| match instruction {
                Instruction::EffectCall { operation, .. } => Some(*operation),
                _ => None,
            })
            .collect::<Vec<_>>();
        for operation in [
            FsOperation::ReadText,
            FsOperation::ReadBytes,
            FsOperation::WriteText,
            FsOperation::WriteBytes,
            FsOperation::List,
            FsOperation::Search,
        ] {
            assert!(operations.contains(&operation));
        }
        assert!(
            compilation
                .module
                .effect_sets
                .contains(&vec!["fs.read".to_owned()])
        );
        assert!(
            compilation
                .module
                .effect_sets
                .contains(&vec!["fs.write".to_owned()])
        );
    }

    #[test]
    fn http_and_external_permission_builtins_have_exact_lazy_signatures() {
        let source = r#"
async fn fetch() returns Result<HttpResponse, NetworkError> effects [net.http_get] {
  await http.get("https://api.example.test/status")
}
async fn request_file() returns Result<Workspace, FileError>
  effects [permission.request_external_fs] {
  await permission.request_file({
    access: ExternalFsAccess.Read,
    path: "/outside/notes.txt",
    reason: "Read the selected notes."
  })
}
async fn request_directory() returns Result<Workspace, FileError>
  effects [permission.request_external_fs] {
  await permission.request_directory({
    access: ExternalFsAccess.ReadWrite,
    path: "/outside/project",
    reason: "Update the selected project.",
    recursive: true
  })
}
export async fn main() returns Void
  effects [net.http_get, permission.request_external_fs] {
  let response = await http.get("https://api.example.test/status");
  let grant = await permission.request_file({
    access: ExternalFsAccess.Write,
    path: "/outside/output.txt",
    reason: "Write the selected output."
  });
  ()
}
"#;
        let compilation = compile_source(source).expect("filesystem builtins compile");
        verify(compilation.module.clone()).expect("filesystem builtin module verifies");
        let operations = compilation
            .module
            .functions
            .iter()
            .flat_map(|function| function.code.iter())
            .filter_map(|instruction| match instruction {
                Instruction::EffectCall { operation, .. } => Some(*operation),
                _ => None,
            })
            .collect::<Vec<_>>();
        for operation in [
            EffectOperation::HttpGet,
            EffectOperation::PermissionRequestFile,
            EffectOperation::PermissionRequestDirectory,
        ] {
            assert!(operations.contains(&operation));
        }
        let http_future = ValueType::Future(Box::new(ValueType::Result(
            Box::new(http_response_type()),
            Box::new(network_error_type()),
        )));
        let permission_future = ValueType::Future(Box::new(ValueType::Result(
            Box::new(ValueType::Workspace),
            Box::new(file_error_type()),
        )));
        for function in &compilation.module.functions {
            for instruction in &function.code {
                let Instruction::EffectCall {
                    destination,
                    operation,
                    ..
                } = instruction
                else {
                    continue;
                };
                let destination_type =
                    &function.registers[usize::try_from(u32::from(*destination)).unwrap()];
                match operation {
                    EffectOperation::HttpGet => assert_eq!(destination_type, &http_future),
                    EffectOperation::PermissionRequestFile
                    | EffectOperation::PermissionRequestDirectory => {
                        assert_eq!(destination_type, &permission_future);
                    }
                    _ => {}
                }
            }
        }
        assert!(
            compilation
                .module
                .constants
                .contains(&Constant::ExternalFsAccess(ExternalFsAccess::Read))
        );
        assert!(
            compilation
                .module
                .constants
                .contains(&Constant::ExternalFsAccess(ExternalFsAccess::ReadWrite))
        );
    }

    #[test]
    fn agent_builtins_are_lazy_exact_and_use_only_named_effects() {
        let source = r#"
async fn message() returns Result<Void, AgentError> effects [agent.message] {
  await agent.message("status")
}
async fn ask() returns Result<String, AgentError> effects [agent.ask] {
  await agent.ask(prompt { system: "continue?", output: String })
}
export async fn main() returns Result<TranscriptSnapshot, AgentError> effects [agent.transcript] {
  await agent.transcript({ limit: 10 })
}
"#;
        let compilation = compile_source(source).expect("agent operations compile");
        verify(compilation.module.clone()).expect("agent module verifies");
        assert_eq!(
            compilation
                .module
                .enum_types
                .iter()
                .filter(|enum_type| enum_type.name == allen_bytecode::TRANSCRIPT_PART_ENUM_NAME)
                .count(),
            1
        );
        let operations = compilation
            .module
            .functions
            .iter()
            .flat_map(|function| &function.code)
            .filter_map(|instruction| match instruction {
                Instruction::EffectCall { operation, .. } => Some(*operation),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            [
                EffectOperation::AgentAsk,
                EffectOperation::AgentTranscript,
                EffectOperation::AgentMessage,
            ]
        );
        for expected in [
            ValueType::Future(Box::new(ValueType::Result(
                Box::new(ValueType::Unit),
                Box::new(agent_error_type()),
            ))),
            ValueType::Future(Box::new(ValueType::Result(
                Box::new(ValueType::String),
                Box::new(agent_error_type()),
            ))),
            ValueType::Future(Box::new(ValueType::Result(
                Box::new(transcript_snapshot_type(0)),
                Box::new(agent_error_type()),
            ))),
        ] {
            assert!(compilation.hir.types.contains(&expected));
        }
    }

    #[test]
    fn sub_agent_builtins_are_lazy_typed_and_opaque() {
        let source = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
async fn create(projection: Projection) returns Result<SubAgent, SubAgentError> effects [sub_agent.create] {
  await sub_agent.create(prompt { system: "seed", output: Void }, projection)
}
async fn run(projection: Projection) returns Result<Bool, SubAgentError> effects [sub_agent.run] {
  await sub_agent.run<Bool>(prompt { system: "run", output: Bool }, projection)
}
async fn message(target: SubAgent) returns Result<Void, SubAgentError> effects [sub_agent.message] {
  await sub_agent.message(target, "status")
}
async fn ask(target: SubAgent) returns Result<Bool, SubAgentError> effects [sub_agent.ask] {
  await sub_agent.ask<Bool>(target, prompt { system: "ask", output: Bool })
}
export async fn main() returns Void { () }
"#;
        let compilation = compile_source(source).expect("sub-agent operations compile");
        verify(compilation.module.clone()).expect("sub-agent module verifies");
        let operations = compilation
            .module
            .functions
            .iter()
            .flat_map(|function| {
                function
                    .code
                    .iter()
                    .map(move |instruction| (function, instruction))
            })
            .filter_map(|(function, instruction)| match instruction {
                Instruction::EffectCall { operation, .. } => {
                    Some((function, *operation, instruction))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for operation in [
            EffectOperation::SubAgentCreate,
            EffectOperation::SubAgentRun,
            EffectOperation::SubAgentMessage,
            EffectOperation::SubAgentAsk,
        ] {
            assert!(operations.iter().any(|(_, actual, _)| *actual == operation));
        }
        for (function, operation, instruction) in operations {
            if matches!(
                operation,
                EffectOperation::SubAgentRun | EffectOperation::SubAgentAsk
            ) {
                assert_eq!(
                    allen_bytecode::typed_response_output_type(function, instruction),
                    Some(&ValueType::Bool)
                );
            }
        }

        let aggregate = r#"
record Bad { child: SubAgent }
export async fn main() returns Void { () }
"#;
        let diagnostics = compile_source(aggregate).expect_err("SubAgent cannot be stored");
        assert_eq!(diagnostics[0].code, "E3011");
    }

    #[test]
    fn collection_literals_construct_exact_sub_agent_projection() {
        let source = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
export async fn main() returns Result<Void, SubAgentError> effects [sub_agent.create, sub_agent.run, sub_agent.message, sub_agent.ask] {
  let projection: Projection = { capabilities: [], limits: map {}, tools: [] };
  let agent = (await sub_agent.create(prompt { system: "seed", output: Void }, projection))?;
  let _message = (await sub_agent.message(agent, "status"))?;
  let _run = (await sub_agent.run<Bool>(prompt { system: "run", output: Bool }, projection))?;
  let _ask = (await sub_agent.ask<Bool>(agent, prompt { system: "ask", output: Bool }))?;
  Ok(())
}
"#;
        let compilation = compile_source(source).expect("projection literals compile");
        verify(compilation.module).expect("collection literal module verifies");
    }

    #[test]
    fn collection_literals_require_sound_types_and_unique_constant_map_keys() {
        for (source, expected) in [
            (
                r#"export async fn main() returns Void { let values = []; () }"#,
                "empty List requires an expected List type",
            ),
            (
                r#"export async fn main() returns Void { let values = [1, "two"]; () }"#,
                "expected Int, found String",
            ),
            (
                r#"export async fn main() returns Void { let values = map {}; () }"#,
                "empty Map requires an expected Map type",
            ),
            (
                r#"export async fn main() returns Void { let values = map { 1: "one", "two": "two" }; () }"#,
                "expected Int, found String",
            ),
            (
                r#"export async fn main() returns Void { let values = map { "same": 1, "same": 2 }; () }"#,
                "duplicate Map key",
            ),
        ] {
            let diagnostics = compile_source(source).expect_err("collection literal is invalid");
            assert_eq!(diagnostics[0].message, expected);
        }
    }

    #[test]
    fn agent_builtins_reject_missing_effects_arguments_and_session_selection() {
        let missing_effect = r#"
export async fn main() returns String {
  await agent.ask(prompt { system: "continue?", output: String })
}
"#;
        let diagnostics = compile_source(missing_effect).expect_err("effect is required");
        assert_eq!(diagnostics[0].code, "E2403");
        assert_eq!(
            diagnostics[0].message,
            "function 'main' requires undeclared effects [agent.ask]"
        );

        for source in [
            r#"export async fn main() returns String effects [agent.ask] { await agent.ask(1) }"#,
            r#"export async fn main() returns TranscriptSnapshot effects [agent.transcript] {
                 await agent.transcript({ limit: "ten" })
               }"#,
            r#"export async fn main() returns TranscriptSnapshot effects [agent.transcript] {
                 await agent.transcript({ limit: 10, session_id: "other" })
               }"#,
            r#"export async fn main() returns TranscriptSnapshot effects [agent.transcript] {
                 await agent.transcript({ limit: 0 })
               }"#,
        ] {
            let diagnostics = compile_source(source).expect_err("agent input is invalid");
            assert!(matches!(diagnostics[0].code, "E3010" | "E3011"));
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn typed_prompts_preserve_segments_and_lower_exact_response_types() {
        let source = r#"
record Review { approved: Bool, reasons: List<String> }
export async fn main() returns Void effects [agent.ask, model.request, user.ask] {
  let agent_prompt = prompt {
    system: "Review using only supplied evidence."
    context: "release-candidate"
    data: { diff: "safe", tests: 12 }
    output: Review
    policy: { max_attempts: 3 }
  };
  let agent_reply = await agent.ask<Review>(agent_prompt);
  let model_reply = await model.request(prompt {
    system: "Classify."
    data: { input: "value" }
    output: Review
  });
  let user_reply = await user.ask<Review>(prompt {
    system: "Choose."
    output: Review
    policy: { max_attempts: 1 }
  });
  ()
}
"#;
        let compilation = compile_source(source).expect("typed prompts compile");
        verify(compilation.module.clone()).expect("typed prompt module verifies");
        let review = ValueType::Record(vec![
            RecordField {
                name: "approved".to_owned(),
                value_type: ValueType::Bool,
            },
            RecordField {
                name: "reasons".to_owned(),
                value_type: ValueType::List(Box::new(ValueType::String)),
            },
        ]);
        let main = &compilation.module.functions[compilation.module.entry as usize];
        let operations = main
            .code
            .iter()
            .filter_map(|instruction| match instruction {
                Instruction::EffectCall {
                    operation,
                    destination,
                    arguments,
                } => Some((*operation, *destination, arguments.as_slice())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            operations
                .iter()
                .map(|(operation, _, _)| *operation)
                .collect::<Vec<_>>(),
            [
                EffectOperation::AgentAsk,
                EffectOperation::ModelRequest,
                EffectOperation::UserAsk,
            ]
        );
        for (operation, destination, arguments) in &operations {
            let error = match operation {
                EffectOperation::AgentAsk => agent_error_type(),
                EffectOperation::ModelRequest => model_error_type(),
                EffectOperation::UserAsk => user_error_type(),
                _ => unreachable!(),
            };
            assert_eq!(
                main.registers[usize::from(*destination)],
                ValueType::Future(Box::new(ValueType::Result(
                    Box::new(review.clone()),
                    Box::new(error),
                )))
            );
            assert_eq!(arguments.len(), 1);
            assert_eq!(
                prompt_output_type(&main.registers[usize::from(arguments[0])]),
                Some(&review)
            );
        }

        let agent_prompt = operations[0].2[0];
        let fields = main
            .code
            .iter()
            .find_map(|instruction| match instruction {
                Instruction::RecordNew {
                    destination,
                    fields,
                } if *destination == agent_prompt => Some(fields),
                _ => None,
            })
            .expect("prompt record construction");
        assert_eq!(fields.len(), 5);
        assert_ne!(fields[1].1, fields[2].1, "context and data stay separate");
        assert!(main.code.iter().any(|instruction| {
            matches!(instruction, Instruction::ToUnknown { destination, .. } if *destination != fields[1].1 && *destination != fields[2].1)
        }));
        assert!(compilation.module.effect_sets.contains(&vec![
            "agent.ask".to_owned(),
            "model.request".to_owned(),
            "user.ask".to_owned(),
        ]));
    }

    #[test]
    fn typed_request_result_output_is_nested_inside_the_domain_envelope() {
        let source = r#"
async fn request() returns Result<Result<Int, Bool>, ModelError> effects [model.request] {
  await model.request<Result<Int, Bool>>(prompt {
    system: "Return a result value."
    output: Result<Int, Bool>
  })
}
export fn main() returns Void { () }
"#;
        let compilation = compile_source(source).expect("nested Result response compiles");
        verify(compilation.module.clone()).expect("nested Result response verifies");
        let nested = ValueType::Result(Box::new(ValueType::Int), Box::new(ValueType::Bool));
        let envelope = ValueType::Result(Box::new(nested.clone()), Box::new(model_error_type()));
        let request = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::request"))
            .expect("request function is lowered");
        let instruction = request
            .code
            .iter()
            .find(|instruction| {
                matches!(
                    instruction,
                    Instruction::EffectCall {
                        operation: EffectOperation::ModelRequest,
                        ..
                    }
                )
            })
            .expect("model request is lowered");
        let Instruction::EffectCall { destination, .. } = instruction else {
            unreachable!()
        };
        assert_eq!(
            request.registers[usize::from(*destination)],
            ValueType::Future(Box::new(envelope)),
        );
        assert_eq!(
            allen_bytecode::typed_response_output_type(request, instruction),
            Some(&nested),
        );
    }

    #[test]
    fn typed_prompt_diagnostics_reject_invalid_contracts() {
        for source in [
            r#"export fn main() returns Void {
                 let request = prompt { system: "x", system: "y", output: String }; ()
               }"#,
            r#"export fn main() returns Void {
                 let request = prompt { system: "x" }; ()
               }"#,
            r#"export fn main() returns Void {
                 let request = prompt { system: "x", mystery: "y", output: String }; ()
               }"#,
            r#"export fn main() returns Void {
                 let request = prompt { system: 1, output: String }; ()
               }"#,
            r#"export fn main() returns Void {
                 let request = prompt { system: "x", output: String, policy: { max_attempts: 0 } }; ()
               }"#,
            r#"export fn main() returns Void {
                 let request = prompt { system: "x", output: String, policy: { max_attempts: 4 } }; ()
               }"#,
            r#"export fn main() returns Void {
                 let request = prompt { system: "x", output: Workspace }; ()
               }"#,
            r#"export fn main() returns Void {
                 let callback = fn(value: Int) returns Int { value };
                 let request = prompt { system: "x", context: callback, output: String }; ()
               }"#,
            r#"export async fn main() returns String effects [model.request] {
                 await model.request<String>(prompt { system: "x", output: Int })
               }"#,
            r#"export async fn main() returns String effects [agent.ask] {
                 await agent.ask<String>("obsolete")
               }"#,
        ] {
            let diagnostics = compile_source(source).expect_err("prompt must be rejected");
            assert_eq!(diagnostics.len(), 1, "{source}");
            assert!(matches!(diagnostics[0].code, "E3005" | "E3010" | "E3011"));
        }
    }

    #[test]
    fn string_agent_ask_is_rejected() {
        let diagnostics = compile_source(
            r#"export async fn main() returns Result<String, AgentError> effects [agent.ask] {
                 await agent.ask("continue?")
               }"#,
        )
        .expect_err("agent.ask requires a structured prompt");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(diagnostics[0].message, "typed request requires Prompt<T>");
    }

    #[test]
    fn agent_effect_singletons_are_interned_inside_combined_effect_sets() {
        let source = r#"
export async fn main() returns Void effects [agent.ask, task.spawn] {
  await {
    let task = spawn agent.ask(prompt { system: "wait", output: String });
    stop("finished")
  }
}
"#;
        let compilation = compile_source(source).expect("combined effects compile");
        verify(compilation.module.clone()).expect("combined effects verify");
        assert!(
            compilation
                .module
                .effect_sets
                .contains(&vec!["agent.ask".to_owned()])
        );
        assert!(
            compilation
                .module
                .effect_sets
                .contains(&vec!["task.spawn".to_owned()])
        );
        assert!(
            compilation
                .module
                .effect_sets
                .contains(&vec!["agent.ask".to_owned(), "task.spawn".to_owned()])
        );
    }

    #[test]
    fn transcript_part_is_synthetic_only_on_use_and_read_only() {
        let plain = compile_source(r#"export fn main() returns Void { () }"#).unwrap();
        assert!(
            plain
                .module
                .enum_types
                .iter()
                .all(|enum_type| enum_type.name != allen_bytecode::TRANSCRIPT_PART_ENUM_NAME)
        );

        let source = r#"
export async fn main() returns Void effects [agent.transcript] {
  let snapshot = await agent.transcript({ limit: 1 });
  let part = TranscriptPart.Text;
  ()
}
"#;
        let diagnostics = compile_source(source).expect_err("standard enum is read-only");
        assert_eq!(diagnostics[0].code, "E3007");
        assert_eq!(
            diagnostics[0].message,
            "TranscriptPart is a read-only standard type"
        );
    }

    #[test]
    fn transcript_part_supports_exhaustive_read_only_variant_matching() {
        let source = r#"
fn kind(part: TranscriptPart) returns String {
  match part {
    TranscriptPart.Text { text: _ } => "text",
    TranscriptPart.Json { value: _ } => "json",
    TranscriptPart.ToolCall { call_id: _, input: _, name: _ } => "tool_call",
    TranscriptPart.ToolResult { call_id: _, is_error: _, output: _ } => "tool_result",
    TranscriptPart.Attachment { content_ref: _, media_type: _, name: _ } => "attachment",
    TranscriptPart.Redacted { reason_code: _ } => "redacted",
    TranscriptPart.Omitted { content_kind: _, count: _ } => "omitted"
  }
}
export async fn main() returns Void effects [agent.transcript] {
  let snapshot = await agent.transcript({ limit: 1 });
  ()
}
"#;
        let compilation = compile_source(source).expect("variant match compiles");
        verify(compilation.module.clone()).expect("variant match verifies");
        let arms = compilation
            .module
            .functions
            .iter()
            .flat_map(|function| &function.code)
            .find_map(|instruction| match instruction {
                Instruction::SwitchEnum { arms, .. } => Some(arms),
                _ => None,
            })
            .unwrap();
        assert_eq!(arms.len(), 7);
        assert!(arms.iter().all(|arm| !arm.bindings.is_empty()));
    }

    #[test]
    fn filesystem_builtins_require_exact_arguments_effects_and_workspace_result() {
        let missing_effect = r#"
export async fn main() returns Result<HttpResponse, NetworkError> {
  await http.get("https://api.example.test/status")
}
"#;
        let diagnostics = compile_source(missing_effect).expect_err("HTTP effect is required");
        assert_eq!(diagnostics[0].code, "E2403");
        assert_eq!(
            diagnostics[0].message,
            "function 'main' requires undeclared effects [net.http_get]"
        );

        let wrong_request = r#"
async fn request_file() returns Result<Workspace, FileError>
  effects [permission.request_external_fs] {
  await permission.request_file({
    access: ExternalFsAccess.Read,
    path: "/outside/notes.txt",
    reason: "Read the selected notes.",
    recursive: false
  })
}
export fn main() returns Void { () }
"#;
        let diagnostics = compile_source(wrong_request).expect_err("file request shape is exact");
        assert_eq!(diagnostics[0].code, "E3011");
        assert!(
            diagnostics[0]
                .message
                .contains("permission.request_file expected")
        );

        let invalid_access = r#"
async fn request_file() returns Result<Workspace, FileError>
  effects [permission.request_external_fs] {
  await permission.request_file({
    access: ExternalFsAccess.Delete,
    path: "/outside/notes.txt",
    reason: "Delete the selected notes."
  })
}
export fn main() returns Void { () }
"#;
        let diagnostics =
            compile_source(invalid_access).expect_err("external access variant must be exact");
        assert_eq!(diagnostics[0].code, "E3007");
        assert_eq!(
            diagnostics[0].message,
            "ExternalFsAccess has no variant 'Delete'"
        );

        let invalid_http_result = r#"
export async fn main() returns Result<String, NetworkError> effects [net.http_get] {
  await http.get("https://api.example.test/status")
}
"#;
        let diagnostics =
            compile_source(invalid_http_result).expect_err("HTTP result shape must be exact");
        assert_eq!(diagnostics[0].code, "E3007");

        let invalid_grant_result = r#"
async fn request_file() returns Result<String, FileError>
  effects [permission.request_external_fs] {
  await permission.request_file({
    access: ExternalFsAccess.Read,
    path: "/outside/notes.txt",
    reason: "Read the selected notes."
  })
}
export fn main() returns Void { () }
"#;
        let diagnostics = compile_source(invalid_grant_result)
            .expect_err("permission result shape must be exact");
        assert_eq!(diagnostics[0].code, "E3007");

        let nested_workspace = r#"
fn invalid() returns Result<List<Workspace>, FileError> { stop("invalid") }
export fn main() returns Void { () }
"#;
        let diagnostics = compile_source(nested_workspace)
            .expect_err("only the exact permission result stores Workspace");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(diagnostics[0].message, "Workspace cannot be stored in List");
    }

    #[test]
    fn workspace_cannot_escape_an_entry_or_an_aggregate() {
        let entry = r#"
export fn main() returns Workspace { fs.workspace() }
"#;
        let diagnostics = compile_source(entry).expect_err("entry cannot return Workspace");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(diagnostics[0].message, "entry main cannot return Workspace");

        let aggregate = r#"
record Wrapped { workspace: Workspace }
export fn main() returns Void { () }
"#;
        let diagnostics = compile_source(aggregate).expect_err("record cannot store Workspace");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "Workspace cannot be stored in a record"
        );

        let tuple = r#"
export fn main() returns Void {
  let workspace = fs.workspace();
  let wrapped = (workspace,);
  ()
}
"#;
        let diagnostics = compile_source(tuple).expect_err("tuple cannot store Workspace");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "Workspace cannot be stored in a tuple"
        );

        let equality = r#"
export fn main() returns Bool {
  let workspace = fs.workspace();
  workspace == workspace
}
"#;
        let diagnostics = compile_source(equality).expect_err("workspace is opaque");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3008");
        assert_eq!(diagnostics[0].message, "type Workspace does not satisfy Eq");
    }

    #[test]
    fn package_bundle_resolves_canonical_package_imports() {
        let bundle = PackageSourceBundle {
            root: "pkg://app@0.1.0/src/main.allen".to_owned(),
            sources: BTreeMap::from([
                (
                    "pkg://app@0.1.0/src/main.allen".to_owned(),
                    r#"
import { value } from "text_utils/src/text.allen";
export fn main() returns Int { value() }
"#
                    .to_owned(),
                ),
                (
                    "pkg://text-utils@1.2.3/src/text.allen".to_owned(),
                    "export fn value() returns Int { 42 }".to_owned(),
                ),
            ]),
            import_targets: BTreeMap::from([(
                (
                    "pkg://app@0.1.0/src/main.allen".to_owned(),
                    "text_utils/src/text.allen".to_owned(),
                ),
                "pkg://text-utils@1.2.3/src/text.allen".to_owned(),
            )]),
            entry_points: vec![PackageEntryPoint {
                module: "pkg://app@0.1.0/src/main.allen".to_owned(),
                function: "main".to_owned(),
            }],
            entry_modules: vec!["pkg://text-utils@1.2.3/src/text.allen".to_owned()],
        };
        let compilation = compile_package_bundle(&bundle).expect("package import resolves");
        verify(compilation.module).expect("package module verifies");
        assert_eq!(
            compilation
                .exported_functions
                .iter()
                .map(|function| (function.module.as_str(), function.function.as_str()))
                .collect::<Vec<_>>(),
            [
                ("pkg://app@0.1.0/src/main.allen", "main"),
                ("pkg://text-utils@1.2.3/src/text.allen", "value"),
            ]
        );
    }

    #[test]
    fn same_named_nominal_enums_stay_distinct_across_packages() {
        let root = "pkg://app@0.1.0/src/main.allen";
        let left = "pkg://left@1.0.0/src/token.allen";
        let right = "pkg://right@1.0.0/src/token.allen";
        let sources = BTreeMap::from([
            (
                root.to_owned(),
                r#"
import { Token as LeftToken, make as make_left } from "left_pkg/src/token.allen";
import { Token as RightToken, make as make_right } from "right_pkg/src/token.allen";
export fn main() returns (LeftToken, RightToken) { (make_left(), make_right()) }
"#
                .to_owned(),
            ),
            (
                left.to_owned(),
                "export enum Token { Only }\nexport fn make() returns Token { Token.Only }".to_owned(),
            ),
            (
                right.to_owned(),
                "export enum Token { Only }\nexport fn make() returns Token { Token.Only }".to_owned(),
            ),
        ]);
        let import_targets = BTreeMap::from([
            (
                (root.to_owned(), "left_pkg/src/token.allen".to_owned()),
                left.to_owned(),
            ),
            (
                (root.to_owned(), "right_pkg/src/token.allen".to_owned()),
                right.to_owned(),
            ),
        ]);
        let bundle = PackageSourceBundle {
            root: root.to_owned(),
            sources: sources.clone(),
            import_targets: import_targets.clone(),
            entry_points: vec![PackageEntryPoint {
                module: root.to_owned(),
                function: "main".to_owned(),
            }],
            entry_modules: vec![left.to_owned(), right.to_owned()],
        };
        let compilation = compile_package_bundle(&bundle).expect("nominal package types compile");
        assert_eq!(compilation.module.enum_types.len(), 2);
        assert_ne!(
            compilation.module.enum_types[0].name,
            compilation.module.enum_types[1].name
        );

        let mut mismatched_sources = sources;
        mismatched_sources.insert(
            root.to_owned(),
            r#"
import { make as make_left } from "left_pkg/src/token.allen";
import { make as make_right } from "right_pkg/src/token.allen";
fn same<T: Eq>(left: T, right: T) returns Bool { left == right }
export fn main() returns Bool { same(make_left(), make_right()) }
"#
            .to_owned(),
        );
        let mismatched = PackageSourceBundle {
            root: root.to_owned(),
            sources: mismatched_sources,
            import_targets,
            entry_points: vec![PackageEntryPoint {
                module: root.to_owned(),
                function: "main".to_owned(),
            }],
            entry_modules: vec![left.to_owned(), right.to_owned()],
        };
        let diagnostics =
            compile_package_bundle(&mismatched).expect_err("package nominal types differ");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3007");
    }

    #[test]
    fn package_local_import_cannot_leave_the_source_directory() {
        let bundle = PackageSourceBundle {
            root: "pkg://app@0.1.0/src/main.allen".to_owned(),
            sources: BTreeMap::from([(
                "pkg://app@0.1.0/src/main.allen".to_owned(),
                r#"
import { value } from "../outside.allen";
export fn main() returns Int { value() }
"#
                .to_owned(),
            )]),
            import_targets: BTreeMap::new(),
            entry_points: vec![PackageEntryPoint {
                module: "pkg://app@0.1.0/src/main.allen".to_owned(),
                function: "main".to_owned(),
            }],
            entry_modules: Vec::new(),
        };
        let diagnostics = compile_package_bundle(&bundle).expect_err("traversal is rejected");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3003");
        assert_eq!(
            diagnostics[0].message,
            "package-local import leaves the package source directory"
        );
    }

    #[test]
    fn package_bundle_selects_a_one_parameter_manifest_entry() {
        let bundle = PackageSourceBundle {
            root: "pkg://app@0.1.0/src/start.allen".to_owned(),
            sources: BTreeMap::from([(
                "pkg://app@0.1.0/src/start.allen".to_owned(),
                "export fn start(input: String) returns String { input }".to_owned(),
            )]),
            import_targets: BTreeMap::new(),
            entry_points: vec![PackageEntryPoint {
                module: "pkg://app@0.1.0/src/start.allen".to_owned(),
                function: "start".to_owned(),
            }],
            entry_modules: Vec::new(),
        };
        let compilation = compile_package_bundle(&bundle).expect("package entry compiles");
        let entry = compilation
            .exported_functions
            .iter()
            .find(|function| function.module == bundle.root && function.function == "start")
            .expect("selected entry metadata");
        assert_eq!(entry.function_id, compilation.module.entry);
        assert_eq!(entry.parameter_types, vec![ValueType::String]);
        assert_eq!(entry.return_type, ValueType::String);
        assert_eq!(entry.parameter_spellings, vec!["String"]);
        assert_eq!(entry.return_spelling, "String");
    }

    #[test]
    fn package_symbols_escape_build_metadata_and_utf8_paths_reversibly() {
        let module = "pkg://app@0.1.0+build.7/src/café.allen";
        let bundle = PackageSourceBundle {
            root: module.to_owned(),
            sources: BTreeMap::from([(
                module.to_owned(),
                "export fn main() returns Void { () }".to_owned(),
            )]),
            import_targets: BTreeMap::new(),
            entry_points: vec![PackageEntryPoint {
                module: module.to_owned(),
                function: "main".to_owned(),
            }],
            entry_modules: Vec::new(),
        };
        let compilation = compile_package_bundle(&bundle).expect("package source compiles");
        verify(compilation.module.clone()).expect("escaped package symbol verifies");
        assert_eq!(
            compilation.module.functions[0].name,
            "pkg/x617070/x302e312e302b6275696c642e37/x737263/x636166c3a9.allen::main"
        );
        assert_eq!(compilation.debug.sources, vec![module.to_owned()]);
        assert_eq!(compilation.exported_functions[0].module, module);
    }

    #[test]
    fn package_symbol_escaping_does_not_collide_for_package_components() {
        let first = "pkg://a@1-2/src/main.allen";
        let second = "pkg://a-1@2/src/main.allen";
        let bundle = PackageSourceBundle {
            root: first.to_owned(),
            sources: BTreeMap::from([
                (
                    first.to_owned(),
                    "export fn main() returns Void { () }".to_owned(),
                ),
                (
                    second.to_owned(),
                    "export fn main() returns Void { () }".to_owned(),
                ),
            ]),
            import_targets: BTreeMap::new(),
            entry_points: vec![
                PackageEntryPoint {
                    module: first.to_owned(),
                    function: "main".to_owned(),
                },
                PackageEntryPoint {
                    module: second.to_owned(),
                    function: "main".to_owned(),
                },
            ],
            entry_modules: Vec::new(),
        };
        let compilation = compile_package_bundle(&bundle).expect("package sources compile");
        verify(compilation.module.clone()).expect("escaped package symbols verify");
        let names = compilation
            .module
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), 2);
        assert!(names.contains("pkg/x61/x312d32/x737263/x6d61696e.allen::main"));
        assert!(names.contains("pkg/x612d31/x32/x737263/x6d61696e.allen::main"));
    }

    #[test]
    fn dynamic_collection_builtins_are_pure_and_match_all_source_modes() {
        let source = r#"
record Item { value: Int }
enum Choice { First Second }
fn bytes_len(value: Bytes) returns Int { length(value) }
export fn main() returns (Int, List<Int>, List<Item>, List<Choice>, List<List<Int>>) {
  let empty: List<Int> = [];
  let original = [1, 2];
  let alias = original;
  let items = [Item { value: 1 }];
  let choices = [Choice.First];
  let nested = [[1]];
  (
    length(list.append(empty, 0)),
    list.set(list.append(alias, 3), 0, 4),
    list.append(items, Item { value: 2 }),
    list.set(choices, 0, Choice.Second),
    list.append(nested, [2])
  )
}
"#;
        let loose = compile_source(source).expect("loose source compiles");
        let root = "pkg://collections@0.1.0/src/main.allen";
        let package = compile_package_bundle(&PackageSourceBundle {
            root: root.to_owned(),
            sources: BTreeMap::from([(root.to_owned(), source.to_owned())]),
            import_targets: BTreeMap::new(),
            entry_points: vec![PackageEntryPoint {
                module: root.to_owned(),
                function: "main".to_owned(),
            }],
            entry_modules: Vec::new(),
        })
        .expect("package source compiles");
        let inline_source = format!(
            "manifest {{\n  language: \"0.1\"\n  entry: main\n  capabilities: []\n}}\n{source}"
        );
        let (_, inline) =
            compile_inline_manifest_source(&inline_source).expect("inline source compiles");

        for compilation in [loose, package, inline] {
            verify(compilation.module.clone()).expect("collection module verifies");
            let counts = compilation
                .module
                .functions
                .iter()
                .flat_map(|function| &function.code)
                .fold(
                    (0, 0, 0),
                    |(length, append, set), instruction| match instruction {
                        Instruction::Length { .. } => (length + 1, append, set),
                        Instruction::ListAppend { .. } => (length, append + 1, set),
                        Instruction::ListSet { .. } => (length, append, set + 1),
                        _ => (length, append, set),
                    },
                );
            assert_eq!(counts, (2, 4, 2));
            assert!(
                compilation
                    .effect_report
                    .iter()
                    .all(|entry| entry.effects.is_empty())
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn closed_errors_safe_and_checked_builtins_evaluate_async_arguments_once_in_order() {
        let source = r#"
async fn collection() returns List<Int> effects [agent.message] { [11] }
async fn index() returns Int effects [agent.message] { 0 }
async fn left() returns Int effects [agent.message] { 40 }
async fn right() returns Int effects [agent.message] { 2 }
export async fn main() returns (Option<Int>, Option<Int>) effects [agent.message] {
  (list.get(await collection(), await index()), int.checked_add(await left(), await right()))
}
"#;
        let compilation = compile_source(source).expect("safe calls with async arguments compile");
        let main = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::main"))
            .expect("main bytecode");
        let call_names = main
            .code
            .iter()
            .filter_map(|instruction| match instruction {
                Instruction::AsyncCall { function, .. } => Some(
                    compilation.module.functions[*function as usize]
                        .name
                        .rsplit("::")
                        .next()
                        .expect("function name"),
                ),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(call_names, ["collection", "index", "left", "right"]);

        let awaited = main
            .code
            .iter()
            .filter_map(|instruction| match instruction {
                Instruction::Await { destination, .. } => Some(*destination),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(awaited.len(), 4);
        let safe_arguments = main
            .code
            .iter()
            .find_map(|instruction| match instruction {
                Instruction::SafeCollectionCall {
                    operation: SafeCollectionOperation::ListGet,
                    arguments,
                    ..
                } => Some(arguments.as_slice()),
                _ => None,
            })
            .expect("list.get bytecode");
        assert_eq!(safe_arguments, &awaited[..2]);
        let checked_arguments = main
            .code
            .iter()
            .find_map(|instruction| match instruction {
                Instruction::CheckedIntCall {
                    operation: CheckedIntOperation::Add,
                    arguments,
                    ..
                } => Some(arguments.as_slice()),
                _ => None,
            })
            .expect("int.checked_add bytecode");
        assert_eq!(checked_arguments, &awaited[2..]);

        let main_mir = compilation
            .mir
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main MIR");
        main_mir.validate_cfg().expect("safe call MIR validates");
        let mir_counts = main_mir
            .blocks
            .iter()
            .flat_map(|block| &block.operations)
            .fold((0, 0, 0, 0), |counts, operation| match operation {
                MirOperation::AsyncCall { .. } => (counts.0 + 1, counts.1, counts.2, counts.3),
                MirOperation::Await { .. } => (counts.0, counts.1 + 1, counts.2, counts.3),
                MirOperation::SafeCollectionOperation { .. } => {
                    (counts.0, counts.1, counts.2 + 1, counts.3)
                }
                MirOperation::CheckedIntOperation { .. } => {
                    (counts.0, counts.1, counts.2, counts.3 + 1)
                }
                _ => counts,
            });
        assert_eq!(mir_counts, (4, 4, 1, 1));

        let verified = verify(compilation.module).expect("safe call bytecode verifies");
        assert_eq!(
            allen_vm::execute(&verified)
                .expect("safe calls execute")
                .to_string(),
            "(Some(11), Some(42))"
        );
    }

    #[test]
    fn dynamic_collection_builtins_reject_wrong_arity_and_types_stably() {
        for (source, message) in [
            (
                "export fn main() returns Int { length() }",
                "length requires exactly 1 argument",
            ),
            (
                "export fn main() returns List<Int> { list.append([1]) }",
                "list.append requires exactly 2 arguments",
            ),
            (
                "export fn main() returns List<Int> { list.set([1], 0) }",
                "list.set requires exactly 3 arguments",
            ),
            (
                "export fn main() returns Int { length(1) }",
                "length requires String, Bytes, List<T>, or Map<K, V>, found Int",
            ),
            (
                "export fn main() returns List<Int> { list.append([1], true) }",
                "expected Int, found Bool",
            ),
            (
                "export fn main() returns List<Int> { list.set([1], true, 2) }",
                "expected Int, found Bool",
            ),
        ] {
            let diagnostics = compile_source(source).expect_err("source must fail");
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(
                diagnostics[0].code,
                if message.starts_with("expected") {
                    "E3010"
                } else {
                    "E3011"
                }
            );
            assert_eq!(diagnostics[0].message, message);
        }
    }

    #[test]
    fn inline_manifest_is_preparsed_without_changing_source_spans() {
        let source = r#"
manifest {
  language: "0.1"
  entry: main
  capabilities: [fs.read(workdir)]
}
export async fn main() returns Result<String, FileError> effects [fs.read] {
  let workspace = fs.workspace();
  await fs.read_text(workspace, "notes.txt")
}
"#;
        let (manifest, compilation) =
            compile_inline_manifest_source(source).expect("inline manifest compiles");
        assert_eq!(
            manifest,
            Some(InlineManifest {
                language: "0.1".to_owned(),
                entry: "main".to_owned(),
                capabilities: vec!["fs.read(workdir)".to_owned()],
                http_origins: Vec::new(),
                tools: Vec::new(),
            })
        );
        verify(compilation.module).expect("canonical inline source verifies");

        let (manifest, unchanged) = extract_inline_manifest("export fn main() returns Void { () }")
            .expect("ordinary source is accepted");
        assert_eq!(manifest, None);
        assert_eq!(unchanged, "export fn main() returns Void { () }");
    }

    #[test]
    fn manifest_extraction_is_independent_of_later_module_failures() {
        let manifest = r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
}
"#;
        for suffix in [
            "export fn broken(\n",
            "export fn main() returns Void { missing_name }\n",
            "export fn broken() returns String { \"unterminated\n",
        ] {
            let source = format!("{manifest}{suffix}");
            let (extracted, unchanged) =
                extract_inline_manifest(&source).expect("later module failure is irrelevant");
            assert_eq!(extracted.expect("leading manifest").entry, "main");
            assert_eq!(unchanged, source);
        }

        let (extracted, unchanged) = extract_inline_manifest("export fn broken(\n")
            .expect("a source without a manifest is classified without lowering its module");
        assert_eq!(extracted, None);
        assert_eq!(unchanged, "export fn broken(\n");
    }

    #[test]
    fn malformed_manifest_diagnostic_is_exact_and_source_qualified() {
        let source = r#"manifest {
  language "0.1"
  entry: main
  capabilities: []
}
export fn later() returns String { "unterminated
"#;
        let diagnostic = extract_inline_manifest(source).expect_err("manifest must be rejected");
        assert_eq!(diagnostic.code, "E3005");
        assert_eq!(diagnostic.message, "expected `:` after `language` (S0101)");
        assert_eq!(diagnostic.span, Span { start: 22, end: 22 });
        assert_eq!(diagnostic.source.as_deref(), Some("inline.allen"));
    }

    #[test]
    fn every_production_compile_route_parses_each_source_once() {
        let loose = "export fn main() returns Void { () }";
        let (compilation, count) =
            syntax_lowering::count_parse_invocations(|| compile_source(loose));
        compilation.expect("loose source compiles");
        assert_eq!(count, 1);

        let inline = r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
}
export fn main() returns Void { () }
"#;
        let (compilation, count) =
            syntax_lowering::count_parse_invocations(|| compile_inline_manifest_source(inline));
        compilation.expect("inline source compiles");
        assert_eq!(count, 1);

        let catalog = FrozenCatalog::freeze(Vec::new(), &allen_schema::CatalogLimits::default())
            .expect("empty catalog");
        let (compilation, count) = syntax_lowering::count_parse_invocations(|| {
            crate::assemble_inline_source(inline, &catalog)
        });
        compilation.expect("catalog-aware inline source compiles");
        assert_eq!(count, 1);

        let (compilation, count) = syntax_lowering::count_parse_invocations(|| {
            let prepared = prepare_source("main.allen", inline).expect("CLI inline prepares");
            compile_prepared_inline_manifest_source(prepared)
        });
        compilation.expect("CLI inline route compiles");
        assert_eq!(count, 1);

        let (compilation, count) = syntax_lowering::count_parse_invocations(|| {
            let prepared = prepare_source("main.allen", loose).expect("CLI root prepares");
            compile_bundle_with_prepared_source(
                "main.allen",
                &BTreeMap::from([("main.allen".to_owned(), loose.to_owned())]),
                prepared,
            )
        });
        compilation.expect("CLI loose route compiles");
        assert_eq!(count, 1);

        let sources = BTreeMap::from([
            (
                "main.allen".to_owned(),
                "import { helper } from \"./support.allen\"; export fn main() returns Void { helper() }"
                    .to_owned(),
            ),
            (
                "support.allen".to_owned(),
                "export fn helper() returns Void { () }".to_owned(),
            ),
        ]);
        let bundle = PackageSourceBundle {
            root: "main.allen".to_owned(),
            sources,
            import_targets: BTreeMap::new(),
            entry_points: vec![PackageEntryPoint {
                module: "main.allen".to_owned(),
                function: "main".to_owned(),
            }],
            entry_modules: vec!["support.allen".to_owned()],
        };
        let (compilation, count) =
            syntax_lowering::count_parse_invocations(|| compile_package_bundle(&bundle));
        compilation.expect("overlapping package roots compile");
        assert_eq!(count, 2, "each distinct package source is parsed once");
    }

    #[test]
    fn inline_manifest_accepts_agent_capabilities_and_compiles_agent_effects() {
        let source = r#"
manifest {
  language: "0.1"
  entry: main
  capabilities: [agent.ask, agent.message, agent.transcript]
}
export async fn main() returns Result<String, AgentError> effects [agent.ask] {
  await agent.ask(prompt { system: "continue?", output: String })
}
"#;
        let (manifest, compilation) =
            compile_inline_manifest_source(source).expect("agent inline manifest compiles");
        assert_eq!(
            manifest.unwrap().capabilities,
            [
                "agent.ask".to_owned(),
                "agent.message".to_owned(),
                "agent.transcript".to_owned(),
            ]
        );
        verify(compilation.module).expect("agent inline module verifies");
    }

    #[test]
    fn stop_is_effect_free_and_terminates_with_never() {
        let source = r#"
export fn main() returns Int { stop("not approved") }
"#;
        let compilation = compile_source(source).expect("stop compiles");
        verify(compilation.module.clone()).expect("stop module verifies");
        assert_eq!(compilation.module.effect_sets, vec![Vec::<String>::new()]);
        assert!(matches!(
            compilation.module.functions[0].code.last(),
            Some(Instruction::Stop { .. })
        ));
        assert!(matches!(
            compilation.mir.functions[0].blocks[0].terminator,
            MirTerminator::Stop { .. }
        ));
    }

    #[test]
    fn stop_is_never_in_a_value_match_arm() {
        let source = r#"
export fn main() returns Int {
  let flag = true;
  match flag { false => 1, true => stop("done") }
}
"#;
        let compilation = compile_source(source).expect("Never coerces to the arm type");
        verify(compilation.module).expect("Never arm module verifies");
    }

    #[test]
    fn stop_inside_await_scope_uses_terminal_cleanup_edge() {
        let source = r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  await {
    let task = spawn number();
    stop("done")
  }
}
"#;
        let compilation = compile_source(source).expect("scoped stop compiles");
        verify(compilation.module.clone()).expect("scoped stop verifies");
        let main = &compilation.module.functions[compilation.module.entry as usize];
        assert!(matches!(main.code.last(), Some(Instruction::Stop { .. })));
        assert!(
            !main
                .code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::TaskScopeExit { .. }))
        );
        let scope = &compilation.mir.functions[compilation.module.entry as usize].task_scopes[0];
        assert_ne!(scope.normal_join, scope.permanent_stop);
    }

    #[test]
    fn affine_task_restrictions_have_stable_diagnostics() {
        let invalid = [
            r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  1
}
"#,
            r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  let moved = task;
  await task
}
"#,
            r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  mut task = spawn number();
  await task
}
"#,
            r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  (task, 1)
}
"#,
            r#"
async fn number() returns Int { 1 }
export async fn main() returns Task<Int> effects [task.spawn] { spawn number() }
"#,
            r#"
async fn number() returns Int { 1 }
async fn escape() returns Task<Int> effects [task.spawn] {
  await { spawn number() }
}
export async fn main() returns Int effects [task.spawn] {
  let task = await escape();
  await task
}
"#,
            r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  let first = await task;
  await task
}
"#,
            r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  let captured = fn() returns Task<Int> { task };
  await task
}
"#,
        ];
        for source in invalid {
            let diagnostics = compile_source(source).expect_err("source is affine-invalid");
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].code, "E3011", "{diagnostics:?}");
        }
    }

    #[test]
    fn await_and_spawn_reject_wrong_contexts_and_operands() {
        for source in [
            "async fn number() returns Int { 1 } export fn main() returns Int { await number() }",
            "export async fn main() returns Int effects [task.spawn] { spawn 1 }",
        ] {
            let diagnostics = compile_source(source).expect_err("source is invalid");
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].code, "E3011", "{diagnostics:?}");
        }
    }

    #[test]
    fn task_ownership_moves_through_returns_and_arguments() {
        let source = r#"
async fn number() returns Int { 1 }
fn start() returns Task<Int> effects [task.spawn] { spawn number() }
async fn consume(task: Task<Int>) returns Int { await task }
export async fn main() returns Int effects [task.spawn] {
  let task = start();
  await consume(task)
}
"#;
        let compilation = compile_source(source).expect("task ownership transfers");
        verify(compilation.module).expect("ownership-transfer module verifies");
        assert!(compilation.mir.functions.iter().any(|function| {
            function
                .ownership
                .iter()
                .any(|entry| entry.state == MirOwnershipState::Returned)
        }));
        assert!(compilation.mir.functions.iter().any(|function| {
            function
                .ownership
                .iter()
                .any(|entry| entry.state == MirOwnershipState::Moved)
        }));
    }

    #[test]
    fn scoped_task_can_transfer_to_an_awaited_async_callee() {
        let source = r#"
async fn number() returns Int { 1 }
async fn consume(task: Task<Int>) returns Int { await task }
export async fn main() returns Int effects [task.spawn] {
  await {
    let task = spawn number();
    await consume(task)
  }
}
"#;
        let compilation = compile_source(source).expect("scoped task transfers");
        let verified = verify(compilation.module).expect("scoped transfer module verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap(),
            allen_vm::Value::Int(1)
        );
    }

    #[test]
    fn match_paths_must_preserve_one_ownership_state() {
        let valid = r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  let flag = true;
  match flag { false => await task, true => await task }
}
"#;
        compile_source(valid).expect("both paths consume the task");

        let invalid = r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  let flag = true;
  match flag { false => await task, true => 1 }
}
"#;
        let diagnostics = compile_source(invalid).expect_err("one path loses a task");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "match paths must leave the same affine ownership state"
        );
    }

    #[test]
    fn must_consume_futures_propagate_through_calls_and_await() {
        let invalid = [
            r"
async fn number() returns Int { 1 }
async fn capture(task: Task<Int>) returns Int { await task }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  let future = capture(task);
  1
}
",
            r"
async fn nested() returns Future<Int> { async_value() }
async fn async_value() returns Int { 1 }
export async fn main() returns Int {
  let future = await nested();
  1
}
",
        ];
        for source in invalid {
            let diagnostics = compile_source(source).expect_err("obligation is lost");
            assert_eq!(diagnostics[0].code, "E3011", "{diagnostics:?}");
            assert!(diagnostics[0].message.contains("affine obligation"));
        }
    }

    #[test]
    fn ordinary_future_returned_by_sync_call_is_discardable() {
        let source = r"
async fn number() returns Int { 1 }
fn identity(future: Future<Int>) returns Future<Int> { future }
export async fn main() returns Int {
  let future = identity(number());
  1
}
";
        let compilation = compile_source(source).expect("ordinary Future is discardable");
        verify(compilation.module).expect("discardable Future module verifies");
    }

    #[test]
    fn task_returned_by_sync_call_is_owned_by_the_current_scope() {
        let valid = r"
async fn number() returns Int { 1 }
fn start() returns Task<Int> effects [task.spawn] { spawn number() }
export async fn main() returns Void effects [task.spawn] {
  await { let task = start(); () }
}
";
        let compilation = compile_source(valid).expect("scope owns returned task");
        let verified = verify(compilation.module).expect("returned task module verifies");
        assert_eq!(allen_vm::execute(&verified).unwrap(), allen_vm::Value::Unit);

        let invalid = r"
async fn number() returns Int { 1 }
fn start() returns Task<Int> effects [task.spawn] { spawn number() }
export async fn main() returns Int effects [task.spawn] { let task = start(); 1 }
";
        let diagnostics = compile_source(invalid).expect_err("live Task cannot be lost");
        assert_eq!(diagnostics[0].code, "E3011");
        assert!(diagnostics[0].message.contains("affine obligation"));
    }

    #[test]
    fn nested_affine_scope_results_require_explicit_await() {
        let invalid = r"
async fn leaf() returns Int { 1 }
async fn nested() returns Future<Int> { leaf() }
export async fn main() returns Int effects [task.spawn] {
  await {
    let task = spawn nested();
    1
  }
}
";
        let diagnostics = compile_source(invalid).expect_err("nested task result is live");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "nested affine result must be awaited before scope exit"
        );
    }

    #[test]
    fn try_error_inside_await_scope_routes_through_normal_join_cleanup() {
        let source = r"
async fn number() returns Int { 1 }
fn fallible() returns Result<Int, Bool> { Err(false) }
export async fn main() returns Result<Int, Bool> effects [task.spawn] {
  await {
    let task = spawn number();
    let value = fallible()?;
    let number = await task;
    Ok(value + number)
  }
}
";
        let compilation = compile_source(source).expect("try scope compiles");
        verify(compilation.module.clone()).expect("try scope bytecode verifies");
        let mir = compilation
            .mir
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main MIR");
        mir.validate_cfg().expect("MIR CFG is structurally valid");
        let error_target = mir
            .blocks
            .iter()
            .find_map(|block| match block.terminator {
                MirTerminator::TryResult { error, .. } => Some(error),
                _ => None,
            })
            .expect("try error edge");
        assert!(
            mir.blocks[error_target as usize]
                .operations
                .iter()
                .any(|operation| {
                    matches!(
                        operation,
                        MirOperation::TaskScopeCleanup {
                            scope: 1,
                            kind: MirCleanupKind::NormalJoin,
                        }
                    )
                })
        );
    }

    #[test]
    fn mir_cfg_validator_rejects_bad_or_disconnected_cleanup_edges() {
        let source = r"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  await { let task = spawn number(); await task }
}
";
        let compilation = compile_source(source).expect("scope compiles");
        let mir = compilation
            .mir
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main MIR");
        mir.validate_cfg().expect("generated MIR validates");

        let mut duplicate = mir.clone();
        let suspend_index = duplicate
            .blocks
            .iter()
            .position(|block| matches!(block.terminator, MirTerminator::Suspend { .. }))
            .expect("suspend block");
        let MirTerminator::Suspend {
            resume,
            exceptional_cancel,
            ..
        } = &mut duplicate.blocks[suspend_index].terminator
        else {
            unreachable!();
        };
        *exceptional_cancel = *resume;
        assert_eq!(
            duplicate.validate_cfg(),
            Err("MIR suspend or scope cleanup edges must be distinct")
        );

        let mut missing_await = mir.clone();
        missing_await.blocks[suspend_index].operations.clear();
        assert_eq!(
            missing_await.validate_cfg(),
            Err("MIR suspend block does not contain its await operation")
        );

        let mut bad_temporary = mir.clone();
        bad_temporary.blocks[suspend_index].operations = vec![MirOperation::Await {
            destination: u32::MAX,
            source: u32::MAX,
        }];
        assert_eq!(
            bad_temporary.validate_cfg(),
            Err("MIR temporary is out of range")
        );

        let mut disconnected = mir.clone();
        disconnected.blocks.push(MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        });
        assert_eq!(
            disconnected.validate_cfg(),
            Err("MIR contains an unreachable block")
        );
    }

    #[test]
    fn module_order_does_not_change_nominal_ids_or_ir() {
        let sources = nominal_sources();
        let forward = compile_bundle("main.allen", &sources).expect("bundle compiles");
        let reverse = sources.into_iter().rev().collect::<BTreeMap<_, _>>();
        let backward = compile_bundle("main.allen", &reverse).expect("bundle compiles");

        assert_eq!(forward, backward);
        assert_eq!(forward.module.enum_types.len(), 2);
        assert_ne!(
            forward.module.enum_types[0].name,
            forward.module.enum_types[1].name
        );
        verify(forward.module).expect("nominal bundle verifies");
    }

    #[test]
    fn generic_eq_accepts_structural_records_and_aggregates() {
        let source = r"
record Point { x: Int, y: Int }

fn same<T: Eq>(left: T, right: T) returns Bool { left == right }

export fn main() returns (Bool, Bool) {
  let left = Point { y: 2, x: 1 };
  let right = Point { x: 1, y: 2 };
  (same(left, right), same((1, true), (1, true)))
}
";
        let compilation = compile_source(source).expect("record Eq compiles");
        verify(compilation.module).expect("record Eq verifies");
    }

    #[test]
    fn same_named_nominal_enums_do_not_unify() {
        let mut sources = nominal_sources();
        sources.insert(
            "main.allen".to_owned(),
            r#"
import { make as make_left } from "./left.allen";
import { make as make_right } from "./right.allen";

fn same<T: Eq>(left: T, right: T) returns Bool { left == right }
export fn main() returns Bool { same(make_left(), make_right()) }
"#
            .to_owned(),
        );
        let diagnostics = compile_bundle("main.allen", &sources).expect_err("types differ");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3007");
    }

    #[test]
    fn generic_instantiation_executes_the_declared_body() {
        let source = r"
fn always<T: Eq>(left: T, right: T) returns Bool { true }
export fn main() returns Bool { always(1, 2) }
";
        let compilation = compile_source(source).expect("generic compiles");
        assert_eq!(compilation.module.functions.len(), 2);
        assert_eq!(
            compilation.module.constants,
            vec![Constant::Int(1), Constant::Int(2), Constant::Bool(true)]
        );
        verify(compilation.module).expect("generic verifies");
    }

    #[test]
    fn closure_effect_unions_are_interned_before_lowering() {
        let source = r"
fn read() returns Int effects [fs.read] { 1 }
fn write() returns Int effects [fs.write] { 2 }
export fn main() returns fn(Int) returns Int effects [fs.read, fs.write] effects [fs.read, fs.write] {
  fn(value: Int) returns Int effects [fs.read, fs.write] { value + read() + write() }
}
";
        let compilation = compile_source(source).expect("effect union compiles");
        assert!(
            compilation
                .module
                .effect_sets
                .contains(&vec!["fs.read".to_owned(), "fs.write".to_owned()])
        );
        verify(compilation.module).expect("effect union verifies");
    }

    #[test]
    fn match_and_try_have_explicit_control_flow() {
        let source = r"
fn choose(flag: Bool) returns Int { match flag { false => 1, true => 2 } }
fn unwrap(value: Result<Int, Bool>) returns Result<Int, Bool> { Ok(value? + 1) }
export fn main() returns Int { choose(true) }
";
        let compilation = compile_source(source).expect("control flow compiles");
        verify(compilation.module.clone()).expect("control flow verifies");
        assert!(compilation.module.functions.iter().any(|function| {
            function
                .code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::BranchBool { .. }))
        }));
        assert!(compilation.module.functions.iter().any(|function| {
            function
                .code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::TryResult { .. }))
        }));
        assert!(compilation.mir.functions.iter().any(|function| {
            function
                .blocks
                .iter()
                .any(|block| matches!(block.terminator, MirTerminator::SwitchBool { .. }))
        }));
        assert!(compilation.mir.functions.iter().any(|function| {
            function
                .blocks
                .iter()
                .any(|block| matches!(block.terminator, MirTerminator::TryResult { .. }))
        }));
        let choose = compilation
            .mir
            .functions
            .iter()
            .find(|function| function.name == "choose")
            .expect("choose MIR");
        let MirTerminator::SwitchBool {
            false_target,
            true_target,
        } = choose
            .blocks
            .iter()
            .find_map(|block| match block.terminator {
                MirTerminator::SwitchBool {
                    false_target,
                    true_target,
                } => Some(MirTerminator::SwitchBool {
                    false_target,
                    true_target,
                }),
                _ => None,
            })
            .expect("bool switch")
        else {
            unreachable!();
        };
        assert!(
            choose.blocks[false_target as usize]
                .operations
                .iter()
                .any(|operation| matches!(operation, MirOperation::Constant { .. }))
        );
        assert!(
            choose.blocks[true_target as usize]
                .operations
                .iter()
                .any(|operation| matches!(operation, MirOperation::Constant { .. }))
        );
    }

    #[test]
    fn enum_match_discriminants_use_canonical_variant_order() {
        let source = r"
enum Choice { First, Second }
fn choose(value: Choice) returns Int {
  match value { Choice.First => 1, Choice.Second => 2 }
}
export fn main() returns Int { choose(Choice.Second) }
";
        let compilation = compile_source(source).expect("enum match compiles");
        verify(compilation.module.clone()).expect("enum match verifies");
        let switch = compilation
            .module
            .functions
            .iter()
            .flat_map(|function| &function.code)
            .find_map(|instruction| match instruction {
                Instruction::SwitchEnum { arms, .. } => Some(arms),
                _ => None,
            })
            .expect("enum switch exists");
        assert_eq!(switch[0].variant, 0);
        assert_eq!(switch[1].variant, 1);
    }

    #[test]
    fn try_result_preserves_success_and_early_error_paths() {
        for (constructor, variant) in [("Ok", 0), ("Err", 1)] {
            let source = format!(
                r"
fn pass(value: Result<Int, Int>) returns Result<Int, Int> {{ Ok(value?) }}
export fn main() returns Result<Int, Int> {{ pass({constructor}(7)) }}
"
            );
            let compilation = compile_source(&source).expect("try path compiles");
            let verified = verify(compilation.module).expect("try path verifies");
            let value = allen_vm::execute(&verified).expect("try path executes");
            let allen_vm::Value::Enum(value) = value else {
                panic!("expected Result enum");
            };
            assert_eq!(value.variant, variant);
        }
    }

    #[test]
    fn result_match_binds_the_selected_payload() {
        for (constructor, expected) in [("Ok", "7"), ("Err", "9")] {
            let source = format!(
                r"
fn make() returns Result<Int, Int> {{ {constructor}(7) }}
export fn main() returns Int {{
  let value = make();
  match value {{ Ok(number) => number, Err(_) => 9 }}
}}
"
            );
            let compilation = compile_source(&source).expect("result match compiles");
            let verified = verify(compilation.module).expect("result match verifies");
            assert_eq!(allen_vm::execute(&verified).unwrap().to_string(), expected);
        }
    }

    #[test]
    fn private_type_import_is_rejected() {
        let sources = BTreeMap::from([
            (
                "main.allen".to_owned(),
                r#"import { Hidden } from "./support.allen";
export fn main() returns Int { 1 }
"#
                .to_owned(),
            ),
            (
                "support.allen".to_owned(),
                "enum Hidden { Value }".to_owned(),
            ),
        ]);
        let diagnostics = compile_bundle("main.allen", &sources).expect_err("type is private");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3003");
    }

    #[test]
    fn generic_eq_rejects_a_record_with_a_callback() {
        let source = r"
record Handler { callback: fn(Int) returns Int }
fn same<T: Eq>(left: T, right: T) returns Bool { left == right }
export fn main() returns Bool {
  let callback = fn(value: Int) returns Int { value };
  let handler = Handler { callback: callback };
  same(handler, handler)
}
";
        let diagnostics = compile_source(source).expect_err("callback is not Eq");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3008", "{diagnostics:?}");
        assert_eq!(diagnostics[0].labels.len(), 1);
    }

    #[test]
    fn recursive_and_mutable_closure_captures_use_e3010() {
        for source in [
            r"
export fn main() returns Int {
  let recur = fn(value: Int) returns Int { recur(value) };
  recur(1)
}
",
            r"
export fn main() returns Int {
  mut offset = 1;
  let add = fn(value: Int) returns Int { value + offset };
  add(1)
}
",
        ] {
            let diagnostics = compile_source(source).expect_err("capture is invalid");
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].code, "E3010");
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn hir_and_mir_have_resolved_ids_and_nested_effects() {
        fn inspect(
            expression: &HirExpr,
            type_count: usize,
            effect_count: usize,
            span_count: usize,
            saw_effectful_call: &mut bool,
        ) {
            assert!((expression.ty as usize) < type_count);
            assert!((expression.effects as usize) < effect_count);
            assert!((expression.span as usize) < span_count);
            match &expression.kind {
                HirExprKind::Variable => assert!(expression.symbol.is_some()),
                HirExprKind::DirectCall(values) => {
                    assert!(expression.symbol.is_some());
                    *saw_effectful_call |= expression.effects != 0;
                    for value in values {
                        inspect(
                            value,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                    }
                }
                HirExprKind::AsyncCall(values) => {
                    assert!(expression.symbol.is_some());
                    for value in values {
                        inspect(
                            value,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                    }
                }
                HirExprKind::EffectCall { arguments, .. }
                | HirExprKind::StringOperation { arguments, .. }
                | HirExprKind::CapabilityInspect { arguments, .. }
                | HirExprKind::SafeCollectionOperation { arguments, .. }
                | HirExprKind::CheckedIntOperation { arguments, .. } => {
                    for value in arguments {
                        inspect(
                            value,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                    }
                }
                HirExprKind::Template(parts) => {
                    for part in parts {
                        match part {
                            HirTemplatePart::Literal { span, .. } => {
                                assert!((*span as usize) < span_count);
                            }
                            HirTemplatePart::Interpolation(value) => inspect(
                                value,
                                type_count,
                                effect_count,
                                span_count,
                                saw_effectful_call,
                            ),
                        }
                    }
                }
                HirExprKind::Closure { captures, body } => {
                    assert!(expression.symbol.is_some());
                    assert!(captures.iter().all(|symbol| *symbol != u32::MAX));
                    inspect(
                        body,
                        type_count,
                        effect_count,
                        span_count,
                        saw_effectful_call,
                    );
                }
                HirExprKind::List(values)
                | HirExprKind::Tuple(values)
                | HirExprKind::Record(values)
                | HirExprKind::Prompt(values)
                | HirExprKind::Binary(values)
                | HirExprKind::ClosureCall(values)
                | HirExprKind::Block(values) => {
                    for value in values {
                        inspect(
                            value,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                    }
                }
                HirExprKind::Map(entries) => {
                    for (key, value) in entries {
                        inspect(
                            key,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                        inspect(
                            value,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                    }
                }
                HirExprKind::FieldGet(value)
                | HirExprKind::Length(value)
                | HirExprKind::Try(value)
                | HirExprKind::ToUnknown(value)
                | HirExprKind::Narrow(value)
                | HirExprKind::Unary(value)
                | HirExprKind::Convert(value)
                | HirExprKind::Assignment(value)
                | HirExprKind::TaskSnapshot(value)
                | HirExprKind::Await(value)
                | HirExprKind::Stop(value)
                | HirExprKind::Return(value) => {
                    inspect(
                        value,
                        type_count,
                        effect_count,
                        span_count,
                        saw_effectful_call,
                    );
                }
                HirExprKind::Spawn { future, .. } => inspect(
                    future,
                    type_count,
                    effect_count,
                    span_count,
                    saw_effectful_call,
                ),
                HirExprKind::AwaitBlock { body, .. } | HirExprKind::Loop { body } => inspect(
                    body,
                    type_count,
                    effect_count,
                    span_count,
                    saw_effectful_call,
                ),
                HirExprKind::ToolCall { input, .. } => inspect(
                    input,
                    type_count,
                    effect_count,
                    span_count,
                    saw_effectful_call,
                ),
                HirExprKind::ListAppend { values, value } => {
                    inspect(
                        values,
                        type_count,
                        effect_count,
                        span_count,
                        saw_effectful_call,
                    );
                    inspect(
                        value,
                        type_count,
                        effect_count,
                        span_count,
                        saw_effectful_call,
                    );
                }
                HirExprKind::ListSet {
                    values,
                    index,
                    value,
                } => {
                    for child in [values.as_ref(), index.as_ref(), value.as_ref()] {
                        inspect(
                            child,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                    }
                }
                HirExprKind::Match { source, arms } => {
                    inspect(
                        source,
                        type_count,
                        effect_count,
                        span_count,
                        saw_effectful_call,
                    );
                    for arm in arms {
                        inspect(
                            arm,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                    }
                }
                HirExprKind::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    inspect(
                        condition,
                        type_count,
                        effect_count,
                        span_count,
                        saw_effectful_call,
                    );
                    inspect(
                        then_branch,
                        type_count,
                        effect_count,
                        span_count,
                        saw_effectful_call,
                    );
                    if let Some(else_branch) = else_branch {
                        inspect(
                            else_branch,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                    }
                }
                HirExprKind::While { condition, body } => {
                    for child in [condition.as_ref(), body.as_ref()] {
                        inspect(
                            child,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                    }
                }
                HirExprKind::For { source, body, .. } => {
                    match source {
                        HirForSource::Iterable(value) => inspect(
                            value,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        ),
                        HirForSource::Range { start, end } => {
                            for child in [start.as_ref(), end.as_ref()] {
                                inspect(
                                    child,
                                    type_count,
                                    effect_count,
                                    span_count,
                                    saw_effectful_call,
                                );
                            }
                        }
                    }
                    inspect(
                        body,
                        type_count,
                        effect_count,
                        span_count,
                        saw_effectful_call,
                    );
                }
                HirExprKind::Index { collection, index } => {
                    for child in [collection.as_ref(), index.as_ref()] {
                        inspect(
                            child,
                            type_count,
                            effect_count,
                            span_count,
                            saw_effectful_call,
                        );
                    }
                }
                HirExprKind::Unit
                | HirExprKind::Int(_)
                | HirExprKind::Float(_)
                | HirExprKind::Bool(_)
                | HirExprKind::String(_)
                | HirExprKind::Bytes(_)
                | HirExprKind::Enum
                | HirExprKind::Break
                | HirExprKind::Continue
                | HirExprKind::WorkspaceGet => {}
            }
        }

        let source = r"
fn read() returns Int effects [fs.read] { 1 }
fn consume(value: Int) returns Int { value }
fn nested() returns Int effects [fs.read] { consume(read()) }
export fn main() returns Int effects [fs.read] {
  let offset = 1;
  let callback = fn(value: Int) returns Int { value + offset };
  callback(nested())
}
";
        let compilation = compile_source(source).expect("IR fixture compiles");
        let mut saw_effectful_call = false;
        for module in &compilation.hir.modules {
            for function in &module.functions {
                assert_ne!(function.symbol, u32::MAX);
                inspect(
                    &function.body,
                    compilation.hir.types.len(),
                    compilation.module.effect_sets.len(),
                    compilation.hir.spans.len(),
                    &mut saw_effectful_call,
                );
            }
        }
        assert!(saw_effectful_call);
        for function in &compilation.mir.functions {
            assert_ne!(function.symbol, u32::MAX);
            assert!(function.blocks.iter().all(|block| {
                !matches!(
                    block.terminator,
                    MirTerminator::Goto { target: u32::MAX }
                        | MirTerminator::SwitchBool {
                            false_target: u32::MAX,
                            ..
                        }
                        | MirTerminator::SwitchBool {
                            true_target: u32::MAX,
                            ..
                        }
                )
            }));
        }
    }

    #[test]
    fn frozen_tool_binding_lowers_to_typed_tool_instruction() {
        let root = "pkg://demo@0.1.0/src/main.allen";
        let bundle = PackageSourceBundle {
            root: root.to_owned(),
            sources: BTreeMap::from([(
                root.to_owned(),
                r#"
export async fn main(value: String) returns Result<String, String>
  effects [tool.github.create_issue@2] {
  await tools.github.create_issue.call({ value })
}
"#
                .to_owned(),
            )]),
            import_targets: BTreeMap::new(),
            entry_points: vec![PackageEntryPoint {
                module: root.to_owned(),
                function: "main".to_owned(),
            }],
            entry_modules: vec![],
        };
        let binding = CompilerToolBinding {
            source_path: vec!["github".to_owned(), "create_issue".to_owned()],
            contract: 0,
            input: ValueType::Record(vec![RecordField {
                name: "value".to_owned(),
                value_type: ValueType::String,
            }]),
            output: ValueType::String,
            declared_error: ValueType::String,
            error: ValueType::String,
            effect: "tool.github.create_issue@2".to_owned(),
            enum_types: vec![],
        };
        let compilation = compile_package_bundle_with_tools(&bundle, &[binding]).unwrap();
        assert!(compilation.module.functions.iter().any(|function| {
            function
                .code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::ToolInvoke { tool: 0, .. }))
        }));
        assert!(compile_package_bundle_with_tools(&bundle, &[]).is_err());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn catalog_preparation_generates_typed_contracts_and_rebases_nominal_enums() {
        let definition = allen_schema::ToolDefinition::parse(
            "example.lookup",
            "2.1.3",
            r#"{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}"#,
            r#"{"oneOf":[{"type":"object","properties":{"tag":{"type":"string","enum":["found"]},"value":{"type":"string"}},"required":["tag","value"],"additionalProperties":false},{"type":"object","properties":{"tag":{"type":"string","enum":["missing"]}},"required":["tag"],"additionalProperties":false}]}"#,
            r#"{"type":"string"}"#,
            vec!["external.read".to_owned()],
            allen_schema::Idempotency::Idempotent,
            &allen_schema::SchemaLimits::default(),
        )
        .unwrap();
        let catalog = allen_schema::FrozenCatalog::freeze(
            vec![definition],
            &allen_schema::CatalogLimits::default(),
        )
        .unwrap();
        let requirements = [ToolRequirement::parse("example.lookup", ">=2.0.0, <3.0.0").unwrap()];
        let mut prepared = prepare_tools(&catalog, &requirements).unwrap();
        assert_eq!(prepared.bindings[0].source_path, ["example", "lookup"]);
        assert_eq!(prepared.bindings[0].effect, "tool.example.lookup@2");
        assert_eq!(prepared.contracts[0].input_schema, 0);
        assert_eq!(prepared.contracts[0].output_schema, 1);
        assert_eq!(prepared.contracts[0].error_schema, 2);
        assert_eq!(prepared.bindings[0].enum_types.len(), 2);
        assert!(
            prepared.bindings[0].enum_types[0]
                .name
                .starts_with("tools.example.lookup.Output_union_")
        );
        assert_eq!(
            prepared.bindings[0].enum_types[1]
                .variants
                .iter()
                .map(|variant| variant.name.as_str())
                .collect::<Vec<_>>(),
            ["Declared", "Unavailable", "Schema"]
        );

        let root = "pkg://demo@0.1.0/src/main.allen";
        let bundle = PackageSourceBundle {
            root: root.to_owned(),
            sources: BTreeMap::from([(
                root.to_owned(),
                r#"
export enum Local { Ready }
export async fn main(value: String) returns String
  effects [tool.example.lookup@2] {
  let outcome = await tools.example.lookup.call({ value });
  "done"
}
"#
                .to_owned(),
            )]),
            import_targets: BTreeMap::new(),
            entry_points: vec![PackageEntryPoint {
                module: root.to_owned(),
                function: "main".to_owned(),
            }],
            entry_modules: vec![],
        };
        let compilation =
            compile_package_bundle_with_prepared_tools(&bundle, &mut prepared).unwrap();
        assert_eq!(compilation.module.enum_types.len(), 3);
        assert!(compilation.module.enum_types[1].name.starts_with(root));
        assert!(compilation.module.enum_types[1].name.contains("::_tool_"));
        assert_eq!(
            prepared.schemas[1].value_type,
            ValueType::Enum(1),
            "artifact schema IDs follow source enum IDs"
        );
        assert_eq!(
            verify(compilation.module.clone()).unwrap_err().message,
            "tool invocation requires a frozen tool catalog"
        );
        let input_schema = prepared.contracts[0].error_schema;
        let output_schema = input_schema;
        let main_id = compilation
            .exported_functions
            .iter()
            .find(|function| function.function == "main")
            .unwrap()
            .function_id;
        let tool_digest = allen_bytecode::compute_tool_contract_digest(&prepared.contracts);
        let artifact = allen_bytecode::Artifact {
            metadata: allen_bytecode::ArtifactMetadata::default(),
            module: compilation.module,
            debug: None,
            schemas: prepared.schemas.clone(),
            entries: vec![allen_bytecode::EntryContract {
                name: "main".to_owned(),
                function: main_id,
                input_schema,
                output_schema,
            }],
            imports: vec![],
            manifest: Some(allen_bytecode::ManifestContract {
                package: "demo".to_owned(),
                version: "0.1.0".to_owned(),
                language_requirement: ">=0.1.0, <0.2.0".to_owned(),
                required_capabilities: vec![],
                optional_capabilities: vec![],
                limits: vec![],
                https_origins: vec![],
                required_tools: prepared.contracts.clone(),
                tool_contract_digest: tool_digest,
            }),
        };
        let bytes = allen_bytecode::encode(&artifact).unwrap();
        allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
            .unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn closed_errors_learnxinyminutes_catalog_compiles_and_runs() {
        let definition = allen_schema::ToolDefinition::parse(
            "demo.echo",
            "1.0.0",
            r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}"#,
            r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}"#,
            r#"{"type":"object","properties":{"code":{"type":"string"},"message":{"type":"string"}},"required":["code","message"],"additionalProperties":false}"#,
            vec![],
            allen_schema::Idempotency::Idempotent,
            &allen_schema::SchemaLimits::default(),
        )
        .unwrap();
        let catalog =
            FrozenCatalog::freeze(vec![definition], &allen_schema::CatalogLimits::default())
                .unwrap();
        let source = include_str!("../../../../examples/learnxinyminutes.allen");
        let (manifest, unchanged_source) = extract_inline_manifest(source).unwrap();
        let manifest = manifest.expect("tutorial has a manifest");
        let mut prepared = prepare_tools(&catalog, &manifest.tools).unwrap();
        let root = "pkg://learnxinyminutes@0.1.0/src/main.allen";
        let support = "pkg://learnxinyminutes@0.1.0/src/functions-and-effects/support.allen";
        let bundle = PackageSourceBundle {
            root: root.to_owned(),
            sources: BTreeMap::from([
                (root.to_owned(), unchanged_source),
                (
                    support.to_owned(),
                    include_str!("../../../../examples/functions-and-effects/support.allen")
                        .to_owned(),
                ),
            ]),
            import_targets: BTreeMap::from([(
                (
                    root.to_owned(),
                    "./functions-and-effects/support.allen".to_owned(),
                ),
                support.to_owned(),
            )]),
            entry_points: vec![PackageEntryPoint {
                module: root.to_owned(),
                function: "main".to_owned(),
            }],
            entry_modules: vec![support.to_owned()],
        };
        let compilation =
            compile_package_bundle_with_prepared_tools(&bundle, &mut prepared).unwrap();
        let main = compilation
            .exported_functions
            .iter()
            .find(|function| function.module == root && function.function == "main")
            .unwrap();
        let return_type = main.return_type.clone();
        let main_id = main.function_id;
        assert_eq!(
            verify(compilation.module.clone()).unwrap_err().message,
            "tool invocation requires a frozen tool catalog"
        );
        let typed_response_schemas = compilation
            .module
            .functions
            .iter()
            .flat_map(|function| {
                function.code.iter().filter_map(|instruction| {
                    allen_bytecode::typed_response_output_type(function, instruction).cloned()
                })
            })
            .collect::<Vec<_>>();
        let mut push_schema = |value_type: ValueType| {
            if let Some(index) = prepared
                .schemas
                .iter()
                .position(|schema| schema.value_type == value_type)
            {
                u32::try_from(index).unwrap()
            } else {
                let index = u32::try_from(prepared.schemas.len()).unwrap();
                prepared
                    .schemas
                    .push(allen_bytecode::StrictSchema { value_type });
                index
            }
        };
        let input_schema = push_schema(ValueType::String);
        let output_schema = push_schema(return_type);
        for output in typed_response_schemas {
            push_schema(output);
        }
        let tool_digest = allen_bytecode::compute_tool_contract_digest(&prepared.contracts);
        let artifact = allen_bytecode::Artifact {
            metadata: allen_bytecode::ArtifactMetadata::default(),
            module: compilation.module,
            debug: None,
            schemas: prepared.schemas,
            entries: vec![allen_bytecode::EntryContract {
                name: "main".to_owned(),
                function: main_id,
                input_schema,
                output_schema,
            }],
            imports: vec![],
            manifest: Some(allen_bytecode::ManifestContract {
                package: "learnxinyminutes".to_owned(),
                version: "0.1.0".to_owned(),
                language_requirement: ">=0.1.0, <0.2.0".to_owned(),
                required_capabilities: vec![],
                optional_capabilities: vec![],
                limits: vec![],
                https_origins: vec![],
                required_tools: prepared.contracts,
                tool_contract_digest: tool_digest,
            }),
        };
        let bytes = allen_bytecode::encode(&artifact).unwrap();
        let verified =
            allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
                .unwrap_or_else(|error| panic!("tutorial artifact failed verification: {error}"));
        let mut clock = allen_vm::SystemMonotonicClock::new();
        let mut observer = TutorialObserver;
        let mut cancellation = TutorialCancellation;
        let mut effects = TutorialEffects;
        let result = allen_vm::execute_entry_with_capabilities_and_runtime_context(
            verified.verified_module(),
            None,
            main_id,
            &[allen_vm::Value::String("tutorial".into())],
            allen_vm::ExecutionLimits::default(),
            &mut clock,
            &mut observer,
            &mut cancellation,
            &mut effects,
            &allen_vm::ExecutionCapabilities::default(),
        );
        assert!(
            matches!(
                result,
                Ok(allen_vm::ExecutionOutcome::Completed(ref result))
                    if matches!(result.value, allen_vm::Value::Record(_))
            ),
            "{result:?}"
        );
    }

    #[test]
    fn inline_manifest_parses_exact_required_tool_records() {
        let source = r#"
manifest {
  language: "0.1"
  entry: main
  capabilities: []
  tools: { required: [
    { name: "release-tools.create-issue", version: ">=2.0.0, <3.0.0" },
    { name: "deploy", version: ">=1.0.0, <2.0.0" }
  ] }
}
export fn main() returns Void { () }
"#;
        let (manifest, _) = compile_inline_manifest_source(source).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(
            manifest
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["deploy", "release-tools.create-issue"]
        );
        for invalid in [
            source.replace("required:", "optional:"),
            source.replace(">=2.0.0, <3.0.0", "^2.0.0"),
            source.replace("release-tools.create-issue", "bad..name"),
            source.replace("version:", "unknown:"),
        ] {
            assert!(extract_inline_manifest(&invalid).is_err());
        }
    }

    #[test]
    fn frontend_literals_operators_conversions_and_indexing_execute() {
        let source = r#"
export fn main() returns String {
  mut total = -1 + 2 * 3;
  total = total + b"A\x42"[1];
  let fraction = -to_float(total) / 2.0;
  let same = b"A\x42" == to_bytes("AB");
  let indexed = [1, 2][0] + map { "value": 3 }["value"] + (4, true)[0];
  to_string(same && !false && fraction < -35.0 && indexed == 8)
}
"#;
        let compilation = compile_source(source).expect("canonical scalar program compiles");
        let verified = verify(compilation.module).expect("canonical scalar program verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap(),
            allen_vm::Value::String("true".into())
        );
    }

    #[test]
    fn frontend_literal_bits_and_escapes_are_exact() {
        let source = r#"
export fn main() returns (Float, Bytes, String, Int, Void) {
  (12.5e-1, b"\"\\\n\r\t\0\b\f\x41", "\"\\\n\r\t\0\b\f", -9223372036854775808, ())
}
"#;
        let compilation = compile_source(source).expect("exact literals compile");
        assert!(
            compilation
                .module
                .constants
                .contains(&Constant::Float(canonical_float_bits(1.25_f64.to_bits())))
        );
        assert!(compilation.module.constants.contains(&Constant::Bytes(vec![
            b'"', b'\\', b'\n', b'\r', b'\t', 0, 8, 12, b'A'
        ])));
        assert!(
            compilation
                .module
                .constants
                .contains(&Constant::String("\"\\\n\r\t\0\u{8}\u{c}".to_owned()))
        );
        assert!(
            compilation
                .module
                .constants
                .contains(&Constant::Int(i64::MIN))
        );
        verify(compilation.module).expect("exact literal module verifies");
    }

    #[test]
    fn frontend_records_enums_patterns_and_unknown_execute() {
        let source = r#"
record Pair { left: Int, right: Int }
enum Shape { Empty, Pair(Int, Int), Named { left: Int, right: Int } }

fn pass(shape: Shape) returns Shape { shape }
fn score(shape: Shape) returns Int {
  match pass(shape) {
    Shape.Empty => 0,
    Shape.Pair(left, right) => left + right,
    Shape.Named { right, left } => left + right
  }
}
fn record_score(pair: Pair) returns Int {
  match pair { Pair { right, left } => left + right }
}
fn structural_score(pair: { left: Int, right: Int }) returns Int {
  pair.left + pair.right
}
fn contextual(values: List<Int>, choice: Option<Int>) returns Int {
  length(values) + match choice { Some(value) => value, None => 0 }
}
fn tuple_contextual(value: (Option<Int>, List<Int>)) returns Int {
  length(value[1]) + match value[0] { Some(number) => number, None => 0 }
}
fn decode(value: unknown) returns Int {
  match narrow<Pair>(value) { Some(pair) => pair.left + pair.right, None => 0 }
}
export fn main() returns Int {
  score(Shape.Named { right: 4, left: 3 })
    + score(Shape.Pair(1, 2))
    + score(Shape.Empty)
    + record_score(Pair { right: 6, left: 5 })
    + structural_score({ right: 2, left: 1 })
    + contextual([], None)
    + tuple_contextual((None, []))
    + decode(to_unknown(Pair { right: 8, left: 7 }))
}
"#;
        let compilation = compile_source(source).expect("canonical aggregate program compiles");
        let verified = verify(compilation.module).expect("canonical aggregate program verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap(),
            allen_vm::Value::Int(39)
        );
    }

    #[test]
    fn frontend_rejects_obsolete_and_unsound_source_forms() {
        let invalid = [
            (
                r#"export fn main() returns Int { let mut value = 1; value }"#,
                "E3005",
                "expected local name (S0101)",
            ),
            (
                r#"enum Choice { First } export fn main() returns Choice { Choice::First }"#,
                "E3005",
                "(S0101)",
            ),
            (
                r#"export fn main() returns String { "\x41" }"#,
                "E0004",
                "malformed literal (S0002)",
            ),
            (
                r#"export fn main() returns Int { let match = 1; match }"#,
                "E3005",
                "expected local name (S0101)",
            ),
            (
                r#"export fn main() returns Int { let value = 1; value = 2; value }"#,
                "E3010",
                "cannot assign to immutable binding 'value'",
            ),
            (
                r#"export fn main() returns Bool { 1 == 1.0 }"#,
                "E3007",
                "binary operands must have one exact type",
            ),
            (
                r#"export fn main() returns Int { 9223372036854775808 }"#,
                "E3005",
                "integer literal magnitude requires unary `-` (S0101)",
            ),
            (
                r#"enum Loop { Next(Loop) } export fn main() returns Void { () }"#,
                "E2012",
                "recursive or invalid enum payload type",
            ),
        ];
        for (source, code, message) in invalid {
            let diagnostics = compile_source(source).expect_err("invalid source was accepted");
            assert_eq!(diagnostics[0].code, code, "{source}");
            assert!(
                diagnostics[0].message.contains(message),
                "{source}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn frontend_output_and_debug_tables_are_deterministic() {
        let sources = BTreeMap::from([
            (
                "main.allen".to_owned(),
                r#"import { answer } from "./support.allen";
export fn main() returns Int { answer() }"#
                    .to_owned(),
            ),
            (
                "support.allen".to_owned(),
                "export fn answer() returns Int { 42 }".to_owned(),
            ),
        ]);
        let first = compile_bundle("main.allen", &sources).expect("first bundle compiles");
        let second = compile_bundle("main.allen", &sources).expect("second bundle compiles");
        assert_eq!(first.module, second.module);
        assert_eq!(first.debug, second.debug);
        assert_eq!(first.hir, second.hir);
        assert_eq!(first.mir, second.mir);
        assert_eq!(first.debug.sources, ["main.allen", "support.allen"]);
        assert_eq!(
            first.debug.locations.len(),
            first
                .module
                .functions
                .iter()
                .map(|function| function.code.len())
                .sum::<usize>()
        );
        assert!(first.debug.locations.iter().all(|location| {
            location.start < location.end
                && usize::try_from(location.source).unwrap() < first.debug.sources.len()
        }));
        verify(first.module).expect("deterministic bundle verifies");
    }

    #[test]
    fn frontend_core_diagnostic_categories_and_spans_are_preserved() {
        for (source, code, text) in [
            (
                "export fn main() returns Int { match true { true => 1 } }",
                "E2015",
                "match true { true => 1 }",
            ),
            (
                "export fn main() returns Int { match true { true => 1, true => 2, false => 0 } }",
                "E2016",
                "true",
            ),
            (
                "export fn main() returns Int { let value = to_unknown(1); value + 1 }",
                "E2018",
                "value",
            ),
        ] {
            let diagnostic = &compile_source(source).expect_err("source must fail")[0];
            assert_eq!(diagnostic.code, code);
            assert_eq!(&source[diagnostic.span.start..diagnostic.span.end], text);
        }
    }

    #[test]
    fn frontend_accepts_trailing_commas_in_comma_separated_forms() {
        let sources = BTreeMap::from([
            (
                "main.allen".to_owned(),
                r#"
import { imported, } from "./support.allen";
fn same<T: Eq,>(left: T, right: T,) returns Bool { left == right }
fn apply(callback: fn(Int,) returns Int, value: Int,) returns Int {
  callback(value,)
}
export fn main() returns Bool {
  let callback = fn(value: Int,) returns Int { value };
  let tuple: (Int,) = (apply(callback, imported(),),);
  let narrowed = match narrow<Int>(to_unknown(1),) {
    Some(value) => value,
    None => 0,
  };
  same(tuple[0], narrowed,)
}
"#
                .to_owned(),
            ),
            (
                "support.allen".to_owned(),
                "export fn imported() returns Int { 1 }".to_owned(),
            ),
        ]);
        let compilation = compile_bundle("main.allen", &sources).expect("trailing commas compile");
        verify(compilation.module).expect("trailing-comma module verifies");

        let manifest = r#"
manifest {
  language: "0.1",
  entry: main,
  capabilities: [agent.ask,],
  http_origins: ["https://example.test",],
  tools: { required: [
    { name: "example.lookup", version: ">=1.0.0, <2.0.0", },
  ], },
}
export fn main() returns Void { () }
"#;
        extract_inline_manifest(manifest).expect("manifest trailing commas parse");
    }

    #[test]
    fn frontend_rejects_noncanonical_effect_versions() {
        for version in ["0", "01", "001"] {
            let source = format!(
                "fn helper() returns Int effects [tool.example.call@{version}] {{ 1 }}\n\
                 export fn main() returns Int {{ 1 }}"
            );
            let diagnostic = &compile_source(&source).expect_err("effect version must fail")[0];
            assert_eq!(diagnostic.code, "E3005");
            assert_eq!(diagnostic.source.as_deref(), Some("main.allen"));
            assert!(diagnostic.message.contains("(S0101)"));
        }
    }

    #[test]
    fn omitted_function_closure_and_callback_type_effects_are_empty_contracts() {
        let pure = r#"
fn apply(callback: fn() returns Int) returns Int { callback() }
export fn main() returns Int {
  let answer = fn() returns Int { 42 };
  apply(answer)
}
"#;
        compile_source(pure).expect("omitted pure contracts compile");

        let source = r#"
fn read() returns Int effects [fs.read] { 1 }
export fn main() returns Int { read() }
"#;
        let diagnostics = compile_source(source).expect_err("empty contract rejects effects");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E2403");
        assert_eq!(
            diagnostics[0].message,
            "function 'main' requires undeclared effects [fs.read]"
        );

        let source = r#"
fn read() returns Int effects [fs.read] { 1 }
export fn main() returns fn() returns Int effects [fs.read] {
  fn() returns Int { read() }
}
"#;
        let diagnostics = compile_source(source).expect_err("empty closure rejects effects");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E2403");
        assert_eq!(
            diagnostics[0].message,
            "closure requires undeclared effects [fs.read]"
        );
    }

    #[test]
    fn frontend_diagnostics_retain_the_originating_module() {
        let parse_sources = BTreeMap::from([
            (
                "main.allen".to_owned(),
                "import { broken } from \"./support.allen\"; export fn main() returns Int { broken() }"
                    .to_owned(),
            ),
            (
                "support.allen".to_owned(),
                "export fn broken() returns Int { @ }".to_owned(),
            ),
        ]);
        let diagnostic = &compile_bundle("main.allen", &parse_sources)
            .expect_err("imported parse error must fail")[0];
        assert_eq!(diagnostic.source.as_deref(), Some("support.allen"));

        let lowering_sources = BTreeMap::from([
            (
                "main.allen".to_owned(),
                "import { broken } from \"./support.allen\"; export fn main() returns Int { broken() }"
                    .to_owned(),
            ),
            (
                "support.allen".to_owned(),
                "export fn broken() returns Int { true }".to_owned(),
            ),
        ]);
        let diagnostic = &compile_bundle("main.allen", &lowering_sources)
            .expect_err("imported lowering error must fail")[0];
        assert_eq!(diagnostic.source.as_deref(), Some("support.allen"));
    }

    #[test]
    fn control_flow_conditionals_execute_one_right_associated_branch() {
        let source = r#"
fn choose(flag: Bool) returns Int {
  if (flag) { 1 } else { 2 }
}
fn chain(first: Bool, second: Bool) returns Int {
  if (first) { 1 } else /* retained association */ if (second) { 2 } else { 3 }
}
export fn main() returns Int {
  choose(true) * 100 + choose(false) * 10 + chain(false, false)
}
"#;
        let compilation = compile_source(source).expect("conditionals compile");
        let verified = verify(compilation.module.clone()).expect("conditional module verifies");
        assert_eq!(
            allen_vm::execute(&verified).expect("conditional program executes"),
            allen_vm::Value::Int(123)
        );
        assert!(compilation.module.functions.iter().any(|function| {
            function
                .code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::BranchBool { .. }))
        }));
        for function in &compilation.mir.functions {
            function.validate_cfg().expect("conditional MIR is valid");
        }

        let skipped_trap =
            compile_source("export fn main() returns Int { if (true) { 7 } else { 1 / 0 } }")
                .expect("skipped trap compiles");
        let verified = verify(skipped_trap.module).expect("skipped-trap module verifies");
        assert_eq!(
            allen_vm::execute(&verified).expect("false branch remains unevaluated"),
            allen_vm::Value::Int(7)
        );
    }

    #[test]
    fn control_flow_void_if_and_bare_return_work_in_sync_and_async_functions() {
        let source = r#"
fn guarded(flag: Bool) returns Void {
  if (flag) { return; }
  let value = 1;
  if (value == 0) { return; }
  return;
}
async fn async_guarded(flag: Bool) returns Void {
  if (flag) { return; } else { () }
  return;
}
export fn main() returns Void {
  let ignored = guarded(false);
  ()
}
"#;
        let compilation = compile_source(source).expect("Void control flow compiles");
        let verified = verify(compilation.module).expect("Void control-flow module verifies");
        assert_eq!(
            allen_vm::execute(&verified).expect("Void control flow executes"),
            allen_vm::Value::Unit
        );
    }

    #[test]
    fn void_is_the_only_named_spelling_for_the_empty_tuple_type() {
        for spelling in ["Void", "()"] {
            let source = format!("export fn main() returns {spelling} {{ () }}");
            let compilation = compile_source(&source).expect("empty tuple type compiles");
            assert_eq!(
                compilation.exported_functions[0].return_type,
                ValueType::Unit
            );
        }

        let diagnostics = compile_source("export fn main() returns Unit { () }")
            .expect_err("the previous source spelling is unsupported");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3005");
        assert_eq!(diagnostics[0].message, "unknown type 'Unit'");
    }

    #[test]
    fn control_flow_conditionals_report_one_source_located_type_error() {
        for (source, expected_text) in [
            ("export fn main() returns Void { if (1) { () } }", "1"),
            ("export fn main() returns Int { if (true) { 1 } }", "{ 1 }"),
            (
                "export fn main() returns Int { if (true) { 1 } else { false } }",
                "{ 1 }",
            ),
            (
                "export fn main() returns Int { if (true) { 1 } else { 2 } let value = 3; value }",
                "if (true) { 1 } else { 2 }",
            ),
            ("export fn main() returns Int { return; }", "return;"),
        ] {
            let diagnostics = compile_source(source).expect_err("invalid conditional must fail");
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E3007", "{source}");
            assert_eq!(
                &source[diagnostics[0].span.start..diagnostics[0].span.end],
                expected_text,
                "{source}"
            );
        }
    }

    #[test]
    fn control_flow_effects_and_affine_ownership_join_conservatively() {
        let effects = r#"
fn read() returns Int effects [fs.read] { 1 }
fn write() returns Int effects [fs.write] { 2 }
fn choose(flag: Bool) returns Int effects [fs.read, fs.write] {
  if (flag) { read() } else { write() }
}
export fn main() returns Int effects [fs.read, fs.write] { choose(true) }
"#;
        let compilation = compile_source(effects).expect("branch effects satisfy the contract");
        assert!(
            compilation
                .module
                .effect_sets
                .contains(&vec!["fs.read".to_owned(), "fs.write".to_owned()])
        );
        verify(compilation.module).expect("effect-union module verifies");

        let valid = r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  if (true) { await task } else { await task }
}
"#;
        let compilation = compile_source(valid).expect("both paths consume the task");
        verify(compilation.module).expect("joined ownership verifies");

        let invalid = r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  if (true) { await task } else { 1 }
}
"#;
        let diagnostics = compile_source(invalid).expect_err("one path leaves the task live");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "conditional paths must leave the same affine ownership state"
        );

        let never_and_condition_move = r#"
async fn number() returns Int { 1 }
async fn consume(task: Task<Int>) returns Bool { let value = await task; value == 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  if (await consume(task)) { 7 } else { stop("not selected") }
}
"#;
        let compilation =
            compile_source(never_and_condition_move).expect("condition move and Never join");
        let verified = verify(compilation.module).expect("Never ownership join verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap(),
            allen_vm::Value::Int(7)
        );
    }

    #[test]
    fn control_flow_conditional_effect_union_is_interned_before_unrelated_later_effects() {
        let source = r#"
fn effect_a() returns Int effects [fs.read] { 1 }
fn effect_b() returns Int effects [fs.write] { 2 }
fn effect_c() returns Void effects [net.http_get] { () }
export fn main() returns Int effects [fs.read, fs.write, net.http_get] {
  let selected = if (true) { effect_a() } else { effect_b() };
  let later = effect_c();
  selected
}
"#;
        let compilation = compile_source(source).expect("subexpression effect union is interned");
        let conditional_effects = vec!["fs.read".to_owned(), "fs.write".to_owned()];
        let conditional_effect_id = u32::try_from(
            compilation
                .module
                .effect_sets
                .iter()
                .position(|effects| effects == &conditional_effects)
                .expect("conditional A/B union"),
        )
        .expect("effect-set ID fits");
        let main = compilation
            .hir
            .modules
            .iter()
            .flat_map(|module| &module.functions)
            .find(|function| function.name == "main")
            .expect("main HIR");
        let HirExprKind::Block(expressions) = &main.body.kind else {
            panic!("main body is a HIR block");
        };
        let conditional = expressions
            .iter()
            .find(|expression| matches!(expression.kind, HirExprKind::If { .. }))
            .expect("conditional HIR expression");
        assert_eq!(conditional.effects, conditional_effect_id);
        assert_eq!(
            compilation.module.effect_sets[main.effects as usize],
            [
                "fs.read".to_owned(),
                "fs.write".to_owned(),
                "net.http_get".to_owned()
            ]
        );
        let main_mir = compilation
            .mir
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main MIR");
        assert!(
            main_mir
                .blocks
                .iter()
                .any(|block| matches!(block.terminator, MirTerminator::SwitchBool { .. }))
        );
        main_mir.validate_cfg().expect("conditional MIR validates");
        verify(compilation.module).expect("subexpression-effect module verifies");
    }

    #[test]
    fn control_flow_declared_callback_superset_propagates_into_conditional_and_body_unions() {
        let source = r#"
fn read() returns Int effects [fs.read] { 1 }
fn net() returns Void effects [net.http_get] { () }
fn helper() returns Int effects [fs.read, fs.write, net.http_get] {
  let callback = fn() returns Int effects [fs.read, fs.write] { read() };
  let selected = if (true) { callback() } else { read() };
  let later = net();
  selected
}
export fn main() returns Int effects [fs.read, fs.write, net.http_get] { helper() }
"#;
        let compilation =
            compile_source(source).expect("declared callback effect unions are interned");
        let conditional_effects = vec!["fs.read".to_owned(), "fs.write".to_owned()];
        let body_effects = vec![
            "fs.read".to_owned(),
            "fs.write".to_owned(),
            "net.http_get".to_owned(),
        ];
        let conditional_effect_id = u32::try_from(
            compilation
                .module
                .effect_sets
                .iter()
                .position(|effects| effects == &conditional_effects)
                .expect("declared callback conditional union"),
        )
        .expect("effect-set ID fits");
        let body_effect_id = u32::try_from(
            compilation
                .module
                .effect_sets
                .iter()
                .position(|effects| effects == &body_effects)
                .expect("helper body union"),
        )
        .expect("effect-set ID fits");
        let helper = compilation
            .hir
            .modules
            .iter()
            .flat_map(|module| &module.functions)
            .find(|function| function.name == "helper")
            .expect("helper HIR");
        assert_eq!(helper.effects, body_effect_id);
        assert_eq!(helper.body.effects, body_effect_id);
        let HirExprKind::Block(expressions) = &helper.body.kind else {
            panic!("helper body is a HIR block");
        };
        let conditional = expressions
            .iter()
            .find(|expression| matches!(expression.kind, HirExprKind::If { .. }))
            .expect("callback conditional HIR");
        assert_eq!(conditional.effects, conditional_effect_id);
        verify(compilation.module).expect("declared callback module verifies");
    }

    #[test]
    fn control_flow_branch_bindings_are_scoped_and_mutable_locals_reconcile() {
        for (condition, expected) in [(true, 1), (false, 0)] {
            let source = format!(
                "export fn main() returns Int {{ mut value = 0; if ({condition}) {{ value = 1; }} value }}"
            );
            let compilation = compile_source(&source).expect("mutable branch assignment compiles");
            let verified = verify(compilation.module).expect("mutable branch module verifies");
            assert_eq!(
                allen_vm::execute(&verified).unwrap(),
                allen_vm::Value::Int(expected)
            );
        }

        let source = "export fn main() returns Int { if (true) { let hidden = 1; () } hidden }";
        let diagnostics = compile_source(source).expect_err("branch local must not escape");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3005");
        assert_eq!(
            &source[diagnostics[0].span.start..diagnostics[0].span.end],
            "hidden"
        );
    }

    #[test]
    fn control_flow_early_return_exits_await_scope_and_maps_control_flow_spans() {
        let no_else_before_tail = r#"
async fn delayed(value: Int) returns Int {
  value
}

export async fn main() returns Int effects [task.spawn] {
  await {
    let task = spawn delayed(42);
    if (true) {
      return 7;
    }
    42
  }
}
"#;
        let compilation =
            compile_source(no_else_before_tail).expect("else-less return before tail compiles");
        let verified = verify(compilation.module).expect("else-less return module verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap(),
            allen_vm::Value::Int(7)
        );

        let source = r#"
async fn number() returns Int { 1 }
export async fn main() returns Void effects [task.spawn] {
  await {
    let task = spawn number();
    if (true) {
      let value = await task;
      return;
    } else {
      let value = await task;
      ()
    }
  }
}
"#;
        let compilation = compile_source(source).expect("scoped early return compiles");
        let main_id = compilation
            .module
            .functions
            .iter()
            .position(|function| function.name.ends_with("::main"))
            .expect("main bytecode function");
        let main = &compilation.module.functions[main_id];
        let branch = main
            .code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::BranchBool { .. }))
            .expect("conditional branch instruction");
        let branch_location = compilation
            .debug
            .locations
            .iter()
            .find(|location| {
                location.function as usize == main_id && location.instruction as usize == branch
            })
            .expect("branch debug location");
        assert_eq!(
            &source[branch_location.start as usize..branch_location.end as usize],
            "true"
        );
        assert!(main.code.windows(2).any(|instructions| matches!(
            instructions,
            [
                Instruction::TaskScopeExit { .. },
                Instruction::Return { .. }
            ]
        )));
        let verified = verify(compilation.module).expect("scoped return module verifies");
        assert_eq!(
            allen_vm::execute(&verified).expect("scoped return executes"),
            allen_vm::Value::Unit
        );

        let nested = r#"
async fn number() returns Int { 1 }
export async fn main() returns Void effects [task.spawn] {
  await {
    let outer = spawn number();
    await {
      let inner = spawn number();
      return;
    }
  }
}
"#;
        let compilation = compile_source(nested).expect("nested scoped return compiles");
        let verified = verify(compilation.module).expect("nested scoped return verifies");
        assert_eq!(allen_vm::execute(&verified).unwrap(), allen_vm::Value::Unit);
    }

    #[test]
    fn loops_execute_in_canonical_order_and_preserve_snapshots() {
        let source = r#"
export fn main() returns Int {
  mut total = 0;
  mut index = 0;
  while (index < 3) {
    total = total + index;
    index = index + 1;
  }
  loop {
    total = total + 10;
    break;
  }
  for value in 1..4 {
    total = total + value;
  }
  mut values = [4, 5];
  for value in values {
    total = total + value;
    values = list.append(values, 9);
  }
  for value in b"\x01\x02" {
    total = total + value;
  }
  for (key, value) in map { 2: 20, 1: 10 } {
    total = total + key + value;
  }
  for (value,) in [(6,)] {
    total = total + value;
  }
  for (_, value) in [(99, 7)] {
    total = total + value;
  }
  total + length(values) * 100
}
"#;
        let compilation = compile_source(source).expect("all canonical loops compile");
        let verified = verify(compilation.module.clone()).expect("loop module verifies");
        assert_eq!(
            allen_vm::execute(&verified).expect("loop module executes"),
            allen_vm::Value::Int(477)
        );
        let main = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::main"))
            .expect("main bytecode");
        assert!(
            main.code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::MapEntryAt { .. }))
        );
        assert!(main.code.iter().any(|instruction| matches!(
            instruction,
            Instruction::Jump { target }
                if (*target as usize) < main.code.len()
        )));
    }

    #[test]
    fn loops_nested_break_continue_and_int_boundary_are_exact() {
        let source = r#"
export fn main() returns Int {
  mut total = 0;
  for outer in 0..4 {
    if (outer == 1) { continue; }
    for inner in 0..4 {
      if (inner == 2) { break; }
      total = total + outer * 10 + inner;
    }
  }
  for value in 9223372036854775805..9223372036854775807 {
    total = total + 1;
  }
  total
}
"#;
        let compilation = compile_source(source).expect("nested loop control compiles");
        let verified = verify(compilation.module).expect("nested loop module verifies");
        assert_eq!(
            allen_vm::execute(&verified).expect("nested loops execute"),
            allen_vm::Value::Int(105)
        );

        let empty = compile_source(
            "export fn main() returns Int { mut count = 0; for _ in 7..7 { count = count + 1; } count }",
        )
        .expect("empty range compiles");
        assert_eq!(
            allen_vm::execute(&verify(empty.module).unwrap()).unwrap(),
            allen_vm::Value::Int(0)
        );
    }

    #[test]
    fn loops_bindings_scopes_and_loop_control_diagnostics_are_stable() {
        for (source, code, message) in [
            (
                "export fn main() returns Void { break; }",
                "E3005",
                "break is only valid inside a loop",
            ),
            (
                "export fn main() returns Void { continue; }",
                "E3005",
                "continue is only valid inside a loop",
            ),
            (
                "export fn main() returns Void { while (1) {} }",
                "E3007",
                "while condition must be Bool, found Int",
            ),
            (
                "export fn main() returns Void { for value in 1 {} }",
                "E3007",
                "for iterable must be String, List<T>, Bytes, or Map<K, V>, found Int",
            ),
            (
                "export fn main() returns Void { for (left, right) in [1] {} }",
                "E3007",
                "tuple loop binding requires a Tuple value, found Int",
            ),
            (
                "export fn main() returns Void { for (value, value) in [(1, 2)] {} }",
                "E3005",
                "duplicate loop binding 'value'",
            ),
        ] {
            let diagnostics = compile_source(source).expect_err("invalid loop must fail");
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert_eq!(diagnostics[0].code, code, "{source}");
            assert_eq!(diagnostics[0].message, message, "{source}");
        }

        let closure = r#"
export fn main() returns Void {
  loop {
    let callback = fn() returns Void { break; };
    break;
  }
}
"#;
        let diagnostic = &compile_source(closure).expect_err("closure loop target is isolated")[0];
        assert_eq!(diagnostic.message, "break is only valid inside a loop");

        let scope = "export fn main() returns Int { for hidden in [1] {} hidden }";
        let diagnostic = &compile_source(scope).expect_err("loop binding does not escape")[0];
        assert_eq!(diagnostic.code, "E3005");
        assert_eq!(&scope[diagnostic.span.start..diagnostic.span.end], "hidden");
    }

    #[test]
    fn loops_loop_bodies_require_void_or_never() {
        for (source, expected_span) in [
            ("export fn main() returns Void { while (true) { 1 } }", "1"),
            (
                "export fn main() returns Void { while (false) { if (true) { 1 } else { 2 } } }",
                "if (true) { 1 } else { 2 }",
            ),
            ("export fn main() returns Void { loop { 1 } }", "1"),
            (
                "export fn main() returns Void { loop { if (true) { 1 } else { 2 } } }",
                "if (true) { 1 } else { 2 }",
            ),
            (
                "export fn main() returns Void { for value in [1] { value } }",
                "value",
            ),
            (
                "export fn main() returns Void { for value in [1] { if (true) { value } else { 2 } } }",
                "if (true) { value } else { 2 }",
            ),
        ] {
            let diagnostics = compile_source(source).expect_err("loop value must not be discarded");
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E3007", "{source}");
            assert_eq!(
                diagnostics[0].message, "loop body must have type Void or Never, found Int",
                "{source}"
            );
            assert_eq!(
                &source[diagnostics[0].span.start..diagnostics[0].span.end],
                expected_span,
                "{source}"
            );
        }

        let accepted = r#"
fn while_void() returns Void { while (false) { () } }
fn while_never() returns Void { while (false) { return; } }
fn loop_void() returns Void { loop { () } }
fn loop_never() returns Void { loop { return; } }
fn for_void() returns Void { for value in [1] { () } }
fn for_never() returns Void { for value in [1] { return; } }
export fn main() returns Void { () }
"#;
        let compilation = compile_source(accepted).expect("Void and Never loop bodies compile");
        verify(compilation.module).expect("Void and Never loop body bytecode verifies");
    }

    #[test]
    fn loops_effects_ownership_and_await_cleanup_are_conservative() {
        let effects = r#"
fn read() returns Void effects [fs.read] { () }
fn write() returns Void effects [fs.write] { () }
export fn main() returns Void effects [fs.read, fs.write] {
  while (false) { let ignored = read(); }
  for _ in [1] { let ignored = write(); }
}
"#;
        let compilation = compile_source(effects).expect("skipped loop effects are inferred");
        assert!(
            compilation
                .module
                .effect_sets
                .contains(&vec!["fs.read".to_owned(), "fs.write".to_owned()])
        );

        let affine = r#"
async fn number() returns Int { 1 }
export async fn main() returns Void {
  let pending = number();
  loop { break; }
}
"#;
        let compilation =
            compile_source(affine).expect("forward entry and immediate break preserve the future");
        verify(compilation.module).expect("break-only loop with a live future verifies");

        let cleanup = r#"
async fn number() returns Int { 7 }
export async fn main() returns Int effects [task.spawn] {
  loop {
    await {
      let task = spawn number();
      let value = await task;
      break;
    }
  }
  9
}
"#;
        let compilation = compile_source(cleanup).expect("break cleans nested await scope");
        let main = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::main"))
            .expect("main bytecode");
        assert!(main.code.windows(2).any(|window| matches!(
            window,
            [Instruction::TaskScopeExit { .. }, Instruction::Jump { .. }]
        )));
        let verified = verify(compilation.module).expect("await cleanup loop verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap(),
            allen_vm::Value::Int(9)
        );
    }

    #[test]
    fn loops_hir_mir_and_debug_spans_retain_loop_structure() {
        let source = "export fn main() returns Void { for value in 0..1 { if (true) { continue; } } }";
        let compilation = compile_source(source).expect("loop structure compiles");
        let main = compilation
            .hir
            .modules
            .iter()
            .flat_map(|module| &module.functions)
            .find(|function| function.name == "main")
            .expect("main HIR");
        let HirExprKind::Block(expressions) = &main.body.kind else {
            panic!("main body is a block");
        };
        let loop_hir = expressions
            .iter()
            .find(|expression| matches!(expression.kind, HirExprKind::For { .. }))
            .expect("typed for HIR");
        assert_eq!(compilation.hir.types[loop_hir.ty as usize], ValueType::Unit);
        let HirExprKind::For { binding, .. } = &loop_hir.kind else {
            unreachable!()
        };
        assert_eq!(binding.elements.len(), 1);
        assert!(binding.elements[0].symbol.is_some());
        assert!((binding.elements[0].span as usize) < compilation.hir.spans.len());
        let mir = compilation
            .mir
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main MIR");
        assert!(
            mir.blocks
                .iter()
                .any(|block| matches!(block.terminator, MirTerminator::SwitchBool { .. }))
        );
        mir.validate_cfg().expect("loop MIR validates");
        assert!(compilation.debug.locations.iter().all(|location| {
            (location.start as usize) <= (location.end as usize)
                && (location.end as usize) <= source.len()
        }));
    }

    #[test]
    fn loops_statically_unreachable_break_does_not_open_an_infinite_loop_exit() {
        let terminating = "export fn main() returns Void { loop { if (false) { break; } } }";
        let compilation = compile_source(terminating).expect("infinite loop compiles");
        verify(compilation.module).expect("infinite loop bytecode verifies");

        let unreachable = "export fn main() returns Void { loop { if (false) { break; } } () }";
        let diagnostics =
            compile_source(unreachable).expect_err("dead break cannot make the tail reachable");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3005");
        assert_eq!(
            diagnostics[0].message,
            "code after non-terminating loop is unreachable"
        );
    }

    #[test]
    fn loops_dead_loop_control_is_terminal_without_an_ownership_edge() {
        let source = r#"
async fn number() returns Int { 1 }
fn dead_break_future(future: Future<Int>) returns Future<Int> {
  loop {
    if (false) { break; }
    return future;
  }
}
fn dead_continue_future(future: Future<Int>) returns Future<Int> {
  loop {
    if (false) { continue; }
    return future;
  }
}
fn dead_break_agent(agent: SubAgent) returns Void {
  loop {
    if (false) { break; }
    return;
  }
}
fn dead_continue_agent(agent: SubAgent) returns Void {
  loop {
    if (false) { continue; }
    return;
  }
}
async fn dead_break_task() returns Int effects [task.spawn] {
  loop {
    if (false) {
      let task = spawn number();
      break;
    }
    return 1;
  }
}
async fn dead_continue_task() returns Int effects [task.spawn] {
  loop {
    if (false) {
      let task = spawn number();
      continue;
    }
    return 1;
  }
}
export async fn main() returns Void { () }
"#;
        let compilation = compile_source(source)
            .expect("source-dead break and continue do not collect ownership edges");
        for function in &compilation.module.functions {
            for (index, instruction) in function.code.iter().enumerate() {
                if let Instruction::Jump { target } = instruction {
                    assert!(
                        (*target as usize) > index,
                        "dead loop control must not create a backward bytecode edge: {function:?}"
                    );
                }
            }
        }
        assert!(
            compilation
                .module
                .functions
                .iter()
                .filter(|function| function.name.contains("dead_"))
                .all(|function| function
                    .code
                    .iter()
                    .any(|instruction| matches!(instruction, Instruction::Stop { .. })))
        );
        for function in &compilation.mir.functions {
            function
                .validate_cfg()
                .expect("dead loop-control MIR remains structurally valid");
        }
        assert!(compilation.debug.locations.iter().all(|location| {
            (location.start as usize) <= (location.end as usize)
                && (location.end as usize) <= source.len()
        }));
        verify(compilation.module).expect("dead loop-control bytecode verifies");
    }

    #[test]
    fn loops_reachable_loop_edges_still_reject_live_handles() {
        let future = r#"
async fn number() returns Int { 1 }
export async fn main() returns Void {
  let future = number();
  loop { continue; }
}
"#;
        let diagnostics =
            compile_source(future).expect_err("reachable continue cannot carry a live Future");
        assert_eq!(diagnostics[0].code, "E3011");
        assert!(diagnostics[0].message.contains("reachable continue"));

        let sub_agent = r#"
fn repeat(agent: SubAgent) returns Void { loop {} }
export fn main() returns Void { () }
"#;
        let diagnostics =
            compile_source(sub_agent).expect_err("reachable backedge cannot carry a SubAgent");
        assert_eq!(diagnostics[0].code, "E3011");
        assert!(diagnostics[0].message.contains("reachable loop back-edge"));

        for control in ["break", "continue"] {
            let task = format!(
                r#"
async fn number() returns Int {{ 1 }}
async fn invalid() returns Void effects [task.spawn] {{
  loop {{
    if (true) {{
      let task = spawn number();
      {control};
    }}
  }}
}}
export async fn main() returns Void {{ () }}
"#
            );
            let diagnostics = compile_source(&task)
                .expect_err("reachable loop control cannot abandon its branch-local Task");
            assert_eq!(diagnostics[0].code, "E3011");
            assert_eq!(
                diagnostics[0].message,
                "loop edge leaves a live affine future, task, or SubAgent obligation"
            );
        }
    }

    #[test]
    fn loops_function_valued_loop_bindings_propagate_exact_effects() {
        let source = r#"
fn read() returns Int effects [fs.read] { 1 }
fn source_effect() returns Bool effects [fs.write] { true }
fn body_effect() returns Void effects [net.http_get] { () }
fn later_effect() returns Void effects [agent.message] { () }
fn helper() returns Int effects [agent.message, fs.read, fs.write, net.http_get] {
  mut total = 0;
  let callbacks = [fn() returns Int effects [fs.read] { read() }];
  for callback in callbacks {
    total = total + callback();
  }
  let direct_callback = fn() returns Int effects [fs.read] { read() };
  for _ in 0..0 {
    total = total + direct_callback();
  }
  let pairs = [("callback", fn() returns Int effects [fs.read] { read() })];
  for (_, callback) in pairs {
    total = total + callback();
  }
  for callback in if (source_effect()) { [fn() returns Int effects [fs.read] { read() }] } else { [fn() returns Int effects [fs.read] { read() }] } {
    total = total + callback();
    let ignored = body_effect();
  }
  let ignored = later_effect();
  total
}
export fn main() returns Int effects [agent.message, fs.read, fs.write, net.http_get] { helper() }
"#;
        let compilation =
            compile_source(source).expect("function-valued loop effects are checked exactly");
        let expected = [
            vec!["fs.read".to_owned()],
            vec!["fs.read".to_owned()],
            vec!["fs.read".to_owned()],
            vec![
                "fs.read".to_owned(),
                "fs.write".to_owned(),
                "net.http_get".to_owned(),
            ],
        ];
        let helper = compilation
            .hir
            .modules
            .iter()
            .flat_map(|module| &module.functions)
            .find(|function| function.name == "helper")
            .expect("helper HIR");
        let HirExprKind::Block(expressions) = &helper.body.kind else {
            panic!("helper body is a block");
        };
        let actual = expressions
            .iter()
            .filter(|expression| matches!(expression.kind, HirExprKind::For { .. }))
            .map(|expression| compilation.module.effect_sets[expression.effects as usize].clone())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        verify(compilation.module).expect("function-valued loop bytecode verifies");
    }

    #[test]
    fn loops_record_fields_and_indexes_preserve_nested_callback_effects() {
        let source = r#"
fn read() returns Int effects [fs.read] { 1 }
record CallbackBox { callbacks: List<fn() returns Int effects [fs.read]> }
record NestedCallbackBox { groups: List<List<fn() returns Int effects [fs.read]>> }
fn field_loop(box: CallbackBox) returns Int effects [fs.read] {
  mut total = 0;
  for callback in box.callbacks { total = total + callback(); }
  total
}
fn nested_loop(box: NestedCallbackBox) returns Int effects [fs.read] {
  mut total = 0;
  for callbacks in box.groups {
    for callback in callbacks { total = total + callback(); }
  }
  total
}
fn indexed_loop(box: NestedCallbackBox) returns Int effects [fs.read] {
  mut total = 0;
  for callback in box.groups[0] { total = total + callback(); }
  let callback = box.groups[1][0];
  total + callback()
}
export fn main() returns Int effects [fs.read] {
  let first: CallbackBox = {
    callbacks: [
      fn() returns Int effects [fs.read] { read() },
      fn() returns Int effects [fs.read] { read() }
    ]
  };
  let nested: NestedCallbackBox = {
    groups: [
      [
        fn() returns Int effects [fs.read] { read() },
        fn() returns Int effects [fs.read] { read() }
      ],
      [fn() returns Int effects [fs.read] { read() }]
    ]
  };
  field_loop(first) + nested_loop(nested) + indexed_loop(nested)
}
"#;
        let compilation = compile_source(source)
            .expect("record-wrapped and indexed callback effects are checked exactly");
        for name in ["field_loop", "nested_loop", "indexed_loop"] {
            let report = compilation
                .effect_report
                .iter()
                .find(|entry| entry.function == name)
                .expect("helper effect report");
            assert_eq!(report.effects, ["fs.read"]);
        }
        let verified = verify(compilation.module).expect("nested callback bytecode verifies");
        assert_eq!(allen_vm::execute(&verified), Ok(allen_vm::Value::Int(8)));

        let invalid = r#"
record Invalid { callbacks: List<fn() returns Future<Int>> }
export fn main() returns Void { () }
"#;
        let diagnostics = compile_source(invalid)
            .expect_err("callback containers must not hide affine values in records");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "future, task, or SubAgent values cannot be stored in a record"
        );

        let unused = r#"
record Unused { callbacks: List<fn() returns Int effects [fs.read]> }
export fn main() returns Void { () }
"#;
        let compilation = compile_source(unused)
            .expect("unused callback record still resolves its exact deferred effect type");
        verify(compilation.module).expect("unused callback record bytecode verifies");
    }

    #[test]
    fn loops_conditional_never_preserves_nested_callback_shapes() {
        let template = r#"
fn read() returns Int effects [fs.read] { 1 }
record CallbackBox { callbacks: List<fn() returns Int effects [fs.read]> }
record NestedCallbackBox { boxes: List<CallbackBox> }
fn field_loop(flag: Bool, box: CallbackBox) returns Int effects [fs.read] {
  mut total = 0;
  for callback in if (flag) { box.callbacks } else { stop("callbacks unavailable") } {
    total = total + callback();
  }
  total
}
fn nested_index(flag: Bool, box: NestedCallbackBox) returns Int effects [fs.read] {
  let callbacks = if (flag) { box.boxes[0].callbacks } else { stop("nested unavailable") };
  let callback = callbacks[0];
  callback()
}
export fn main() returns Int effects [fs.read] {
  let box: NestedCallbackBox = {
    boxes: [{
      callbacks: [
        fn() returns Int effects [fs.read] { read() },
        fn() returns Int effects [fs.read] { read() }
      ]
    }]
  };
  field_loop($FLAG, box.boxes[0]) + nested_index($FLAG, box)
}
"#;
        for flag in [true, false] {
            let source = template.replace("$FLAG", if flag { "true" } else { "false" });
            let compilation = compile_source(&source)
                .expect("Never branch preserves the surviving nested callback shape");
            for name in ["field_loop", "nested_index"] {
                let report = compilation
                    .effect_report
                    .iter()
                    .find(|entry| entry.function == name)
                    .expect("helper effect report");
                assert_eq!(report.effects, ["fs.read"], "{name}");

                let hir = compilation
                    .hir
                    .modules
                    .iter()
                    .flat_map(|module| &module.functions)
                    .find(|function| function.name == name)
                    .expect("helper HIR");
                assert_eq!(
                    compilation.module.effect_sets[hir.effects as usize],
                    ["fs.read"],
                    "{name}"
                );

                let bytecode = compilation
                    .module
                    .functions
                    .iter()
                    .find(|function| function.name.ends_with(name))
                    .expect("helper bytecode");
                assert_eq!(
                    compilation.module.effect_sets[bytecode.effects as usize],
                    ["fs.read"],
                    "{name}"
                );
            }
            let verified = verify(compilation.module)
                .expect("conditional callback collection bytecode verifies");
            if flag {
                assert_eq!(allen_vm::execute(&verified), Ok(allen_vm::Value::Int(3)));
            } else {
                assert_eq!(
                    allen_vm::execute(&verified),
                    Err(allen_vm::VmError::Stopped {
                        reason: "callbacks unavailable".to_owned()
                    })
                );
            }
        }

        let incompatible = r#"
fn read() returns Int effects [fs.read] { 1 }
fn write() returns Int effects [fs.write] { 2 }
export fn main() returns Int effects [fs.read, fs.write] {
  let callback = if (true) {
    fn() returns Int effects [fs.read] { read() }
  } else {
    fn() returns Int effects [fs.write] { write() }
  };
  callback()
}
"#;
        let diagnostics = compile_source(incompatible)
            .expect_err("incompatible concrete callback shapes remain a type error");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3007");
        assert_eq!(
            diagnostics[0].message,
            "if branches must have one exact result type"
        );
    }

    #[test]
    fn loops_literal_true_while_controls_fallthrough_exactly() {
        let terminating = "export fn main() returns Int { while (true) {} }";
        let compilation =
            compile_source(terminating).expect("literal true loop terminates control");
        let main = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::main"))
            .expect("main bytecode");
        assert!(main.code.iter().any(|instruction| matches!(
            instruction,
            Instruction::BranchBool {
                false_target,
                true_target,
                ..
            } if false_target == true_target
        )));
        verify(compilation.module).expect("literal true loop bytecode verifies");

        let unreachable = "export fn main() returns Void { while (true) {} () }";
        let diagnostics =
            compile_source(unreachable).expect_err("literal true loop cannot fall through");
        assert_eq!(diagnostics[0].code, "E3005");
        assert_eq!(
            diagnostics[0].message,
            "code after non-terminating loop is unreachable"
        );

        let breaking = r#"
export fn main() returns Int {
  mut value = 0;
  while (true) {
    value = value + 1;
    if (value == 2) { break; }
  }
  value
}
"#;
        let compilation = compile_source(breaking).expect("reachable break restores fallthrough");
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Int(2))
        );

        let nonliteral = r#"
fn choose(flag: Bool) returns Int {
  while (flag) { return 1; }
  2
}
export fn main() returns Int { choose(false) }
"#;
        let compilation =
            compile_source(nonliteral).expect("nonliteral while may execute zero times");
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Int(2))
        );
    }

    #[test]
    fn loops_completed_await_scope_drops_sub_agent_before_later_backedge() {
        let source = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
export async fn main() returns Int effects [sub_agent.create] {
  let ignored = await {
    let projection: Projection = { capabilities: [], limits: map {}, tools: [] };
    let agent = await sub_agent.create(prompt { system: "seed", output: Void }, projection);
    ()
  };
  mut value = 0;
  while (value < 2) { value = value + 1; }
  value
}
"#;
        let compilation = compile_source(source)
            .expect("completed await scope removes its consumed SubAgent binding");
        verify(compilation.module).expect("later backedge ignores dead await-scope registers");
    }

    #[test]
    fn loops_closures_reject_direct_and_structurally_hidden_sub_agent_captures() {
        let direct = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
export async fn main() returns Void effects [sub_agent.create] {
  let callback = await {
    let projection: Projection = { capabilities: [], limits: map {}, tools: [] };
    let agent = await sub_agent.create(prompt { system: "seed", output: Void }, projection);
    fn() returns SubAgent { agent }
  };
  let escaped = callback();
  ()
}
"#;
        let diagnostics = compile_source(direct).expect_err("scoped SubAgent capture is rejected");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "closure cannot capture SubAgent-containing binding 'agent'"
        );

        let hidden = r#"
fn wrap(source: fn() returns SubAgent) returns Void {
  let callback = fn() returns Void { let agent = source(); () };
  ()
}
export fn main() returns Void { () }
"#;
        let diagnostics =
            compile_source(hidden).expect_err("function-shaped SubAgent capture is rejected");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "closure cannot capture SubAgent-containing binding 'source'"
        );

        let ordinary = r#"
fn make(value: Int) returns fn() returns Int { fn() returns Int { value } }
export fn main() returns Int {
  let callback = make(7);
  callback()
}
"#;
        let compilation = compile_source(ordinary).expect("ordinary closure capture is preserved");
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Int(7))
        );
    }

    #[test]
    fn loops_loop_entry_allows_terminal_ownership_transfers() {
        let terminal = r#"
async fn number() returns Int { 1 }
fn transfer_task(task: Task<Int>) returns Task<Int> { loop { return task; } }
fn transfer_future(future: Future<Int>) returns Future<Int> { loop { return future; } }
fn transfer_agent(agent: SubAgent) returns SubAgent { loop { return agent; } }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  let transferred_task = transfer_task(task);
  let first = await transferred_task;
  let future = transfer_future(number());
  let second = await future;
  first + second
}
"#;
        let compilation =
            compile_source(terminal).expect("terminal loops may receive and transfer live handles");
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Int(2))
        );
    }

    #[test]
    fn loops_constant_if_dead_paths_do_not_repeat_live_handles() {
        let source = r#"
async fn number() returns Int { 1 }
fn transfer_future(future: Future<Int>) returns Future<Int> {
  loop { if (true) { return future; } }
}
fn transfer_nested_future(future: Future<Int>) returns Future<Int> {
  loop {
    if (true) {
      if (true) { return future; }
    }
  }
}
fn transfer_task(task: Task<Int>) returns Task<Int> {
  loop { if (true) { return task; } else { loop {} } }
}
fn transfer_agent(agent: SubAgent) returns SubAgent {
  loop { if (false) { loop {} } else { return agent; } }
}
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  let transferred = transfer_task(task);
  let first = await transferred;
  let future = transfer_future(number());
  let second = await future;
  let nested_future = transfer_nested_future(number());
  first + second + await nested_future
}
"#;
        let compilation = compile_source(source)
            .expect("constant dead branches do not create live-handle repeat edges");
        for function in compilation.module.functions.iter().filter(|function| {
            [
                "transfer_future",
                "transfer_nested_future",
                "transfer_task",
                "transfer_agent",
            ]
            .iter()
            .any(|name| function.name.ends_with(name))
        }) {
            for (index, instruction) in function.code.iter().enumerate() {
                if let Instruction::Jump { target } = instruction {
                    assert!(
                        (*target as usize) > index,
                        "constant-dead bytecode path created a loop backedge: {function:?}"
                    );
                }
            }
        }
        for function in compilation.mir.functions.iter().filter(|function| {
            matches!(
                function.name.as_str(),
                "transfer_future" | "transfer_nested_future" | "transfer_task" | "transfer_agent"
            )
        }) {
            for (index, block) in function.blocks.iter().enumerate() {
                if let MirTerminator::Goto { target } = block.terminator {
                    assert!(
                        (target as usize) > index,
                        "constant-dead MIR path created a loop backedge: {function:?}"
                    );
                }
            }
            function.validate_cfg().expect("constant-if MIR validates");
        }
        assert!(compilation.debug.locations.iter().all(|location| {
            (location.start as usize) <= (location.end as usize)
                && (location.end as usize) <= source.len()
        }));
        let verified = verify(compilation.module).expect("constant-if bytecode verifies");
        assert_eq!(allen_vm::execute(&verified), Ok(allen_vm::Value::Int(3)));
    }

    #[test]
    fn loops_repeat_edges_reject_live_handles_and_preserve_exit_joins() {
        let zero_and_break = r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  loop { break; }
  let first = await task;
  let future = number();
  while (false) {}
  let second = await future;
  first + second
}
"#;
        let compilation = compile_source(zero_and_break)
            .expect("zero-iteration and break joins preserve their entry ownership");
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Int(2))
        );

        let future_backedge = r#"
async fn number() returns Int { 1 }
export async fn main() returns Void {
  let future = number();
  loop { continue; }
}
"#;
        let diagnostics =
            compile_source(future_backedge).expect_err("continue cannot carry a live Future");
        assert_eq!(diagnostics[0].code, "E3011");
        assert!(diagnostics[0].message.contains("reachable continue"));

        let task_backedge = r#"
async fn number() returns Int { 1 }
export async fn main() returns Void effects [task.spawn] {
  let task = spawn number();
  loop {}
}
"#;
        let diagnostics =
            compile_source(task_backedge).expect_err("fallthrough cannot carry a live Task");
        assert_eq!(diagnostics[0].code, "E3011");
        assert!(diagnostics[0].message.contains("reachable loop back-edge"));

        let mismatched_zero_iteration = r#"
async fn number() returns Int { 1 }
export async fn main() returns Int effects [task.spawn] {
  let task = spawn number();
  let condition = false;
  while (condition) {
    let ignored = await task;
    break;
  }
  await task
}
"#;
        let diagnostics = compile_source(mismatched_zero_iteration)
            .expect_err("zero-iteration and break exits must preserve one ownership state");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "break must preserve the loop's affine ownership state"
        );
    }

    #[test]
    fn loops_scope_exit_invalidates_only_sub_agents_crossing_that_exit() {
        let valid = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
async fn break_nested() returns Int effects [sub_agent.create] {
  loop {
    await {
      await {
        let projection: Projection = { capabilities: [], limits: map {}, tools: [] };
        let agent = await sub_agent.create(prompt { system: "seed", output: Void }, projection);
        break;
      }
    }
  }
  7
}
async fn continue_scoped() returns Void effects [sub_agent.create] {
  loop {
    await {
      let projection: Projection = { capabilities: [], limits: map {}, tools: [] };
      let agent = await sub_agent.create(prompt { system: "seed", output: Void }, projection);
      continue;
    }
  }
}
async fn return_scoped() returns Int effects [sub_agent.create] {
  loop {
    await {
      let projection: Projection = { capabilities: [], limits: map {}, tools: [] };
      let agent = await sub_agent.create(prompt { system: "seed", output: Void }, projection);
      return 9;
    }
  }
}
export async fn main() returns Void { () }
"#;
        let compilation = compile_source(valid)
            .expect("structured scope exits invalidate their local SubAgent bindings");
        verify(compilation.module).expect("scoped break, continue, and return bytecode verifies");

        let invalid = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
export async fn main() returns Void effects [sub_agent.create] {
  await {
    let projection: Projection = { capabilities: [], limits: map {}, tools: [] };
    let agent = await sub_agent.create(prompt { system: "seed", output: Void }, projection);
    loop { continue; }
  }
}
"#;
        let diagnostics = compile_source(invalid)
            .expect_err("available SubAgent cannot cross an in-scope continue");
        assert_eq!(diagnostics[0].code, "E3011");
        assert!(diagnostics[0].message.contains("reachable continue"));
    }

    #[test]
    fn loops_sub_agent_assignment_preserves_lexical_scope_provenance() {
        let direct_escape = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
export async fn main() returns Void effects [sub_agent.create] {
  let projection: Projection = { capabilities: [], limits: map {}, tools: [] };
  mut selected = await sub_agent.create(prompt { system: "root", output: Void }, projection);
  let ignored = await {
    let local = await sub_agent.create(prompt { system: "local", output: Void }, projection);
    selected = local;
    ()
  };
  ()
}
"#;
        let nested_escape = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
export async fn main() returns Void effects [sub_agent.create] {
  let projection: Projection = { capabilities: [], limits: map {}, tools: [] };
  let ignored = await {
    mut outer = await sub_agent.create(prompt { system: "outer", output: Void }, projection);
    let ignored = await {
      let inner = await sub_agent.create(prompt { system: "inner", output: Void }, projection);
      let alias = inner;
      outer = alias;
      ()
    };
    ()
  };
  ()
}
"#;
        for (source, escaped_name) in [(direct_escape, "local"), (nested_escape, "alias")] {
            let diagnostics = compile_source(source)
                .expect_err("narrower-scope SubAgent assignment must fail in the frontend");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E3011");
            assert_eq!(
                diagnostics[0].message,
                "SubAgent-containing value cannot escape its lexical scope through assignment"
            );
            assert_eq!(
                &source[diagnostics[0].span.start..diagnostics[0].span.end],
                escaped_name
            );
        }

        let valid = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
export async fn main() returns Void effects [sub_agent.create, sub_agent.message] {
  let projection: Projection = { capabilities: [], limits: map {}, tools: [] };
  mut selected = match await sub_agent.create(prompt { system: "selected", output: Void }, projection) { Ok(agent) => agent, Err(_) => stop("unavailable") };
  let replacement = match await sub_agent.create(prompt { system: "replacement", output: Void }, projection) { Ok(agent) => agent, Err(_) => stop("unavailable") };
  selected = replacement;
  let root_message = await sub_agent.message(selected, "root");
  let nested_scope = await {
    let scoped = match await sub_agent.create(prompt { system: "scoped", output: Void }, projection) { Ok(agent) => agent, Err(_) => stop("unavailable") };
    mut nested = scoped;
    let scoped_replacement = match await sub_agent.create(prompt { system: "scoped replacement", output: Void }, projection) { Ok(agent) => agent, Err(_) => stop("unavailable") };
    nested = scoped_replacement;
    let nested_message = await sub_agent.message(nested, "nested");
    ()
  };
  ()
}
"#;
        let compilation = compile_source(valid)
            .expect("same-scope and outer-scope SubAgent assignments remain valid");
        verify(compilation.module)
            .expect("valid SubAgent assignment provenance verifies after scope invalidation");
    }

    #[test]
    fn loops_unbounded_loops_honor_stop_failure_and_instruction_budgets() {
        let stopped =
            compile_source("export fn main() returns Void { loop { stop(\"loop stopped\") } }")
                .expect("stopping loop compiles");
        assert_eq!(
            allen_vm::execute(&verify(stopped.module).unwrap()),
            Err(allen_vm::VmError::Stopped {
                reason: "loop stopped".to_owned()
            })
        );

        let failed = compile_source("export fn main() returns Void { loop { let ignored = 1 / 0; } }")
            .expect("failing loop compiles");
        assert_eq!(
            allen_vm::execute(&verify(failed.module).unwrap()),
            Err(allen_vm::VmError::DivisionByZero)
        );

        let bounded = compile_source(
            "export fn main() returns Void { mut value = 0; loop { value = value + 1; } }",
        )
        .expect("budget-bounded loop compiles");
        let bounded = verify(bounded.module).expect("budget-bounded loop verifies");
        assert_eq!(
            allen_vm::execute_with_limits(
                &bounded,
                allen_vm::ExecutionLimits {
                    instructions: 12,
                    ..allen_vm::ExecutionLimits::default()
                }
            ),
            Err(allen_vm::VmError::ResourceLimit {
                resource: "instructions"
            })
        );
    }

    #[test]
    fn loops_nested_terminal_regions_are_parented_and_preserve_shapes() {
        let loops = r#"
fn while_if(flag: Bool) returns Int {
  while (false) { if (flag) { stop("while if true") } else { stop("while if false") } }
  1
}
fn while_match(flag: Bool) returns Int {
  while (false) { match flag { false => stop("while match false"), true => stop("while match true") } }
  2
}
fn for_if(flag: Bool) returns Int {
  for _ in 0..0 { if (flag) { stop("for if true") } else { stop("for if false") } }
  3
}
fn for_match(flag: Bool) returns Int {
  for _ in 0..0 { match flag { false => stop("for match false"), true => stop("for match true") } }
  4
}
export fn main() returns Int {
  while_if(true) + while_match(false) + for_if(true) + for_match(false)
}
"#;
        let compilation = compile_source(loops).expect("terminal child regions compile in loops");
        for function in &compilation.mir.functions {
            function
                .validate_cfg()
                .expect("nested terminal loop MIR is fully reachable");
        }
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Int(10))
        );

        let callbacks = r#"
fn read() returns Int effects [fs.read] { 1 }
record CallbackBox { callbacks: List<fn() returns Int effects [fs.read]> }
fn from_if(flag: Bool, box: CallbackBox) returns Int effects [fs.read] {
  let callbacks = if (flag) {
    box.callbacks
  } else {
    if (flag) { stop("nested if true") } else { stop("nested if false") }
  };
  let callback = callbacks[0];
  callback()
}
fn from_match(flag: Bool, box: CallbackBox) returns Int effects [fs.read] {
  let callbacks = if (flag) {
    box.callbacks
  } else {
    match flag { false => stop("nested match false"), true => stop("nested match true") }
  };
  let callback = callbacks[0];
  callback()
}
export fn main() returns Int effects [fs.read] {
  let box: CallbackBox = { callbacks: [fn() returns Int effects [fs.read] { read() }] };
  from_if(true, box) + from_match(true, box)
}
"#;
        let compilation =
            compile_source(callbacks).expect("nested all-Never branches retain callback shapes");
        for name in ["from_if", "from_match"] {
            let report = compilation
                .effect_report
                .iter()
                .find(|entry| entry.function == name)
                .expect("callback helper effect report");
            assert_eq!(report.effects, ["fs.read"], "{name}");
        }
        for function in &compilation.mir.functions {
            function
                .validate_cfg()
                .expect("nested callback conditional MIR validates");
        }
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Int(2))
        );
        for (call, reason) in [
            ("from_if(false, box)", "nested if false"),
            ("from_match(false, box)", "nested match false"),
        ] {
            let stopped = callbacks.replace("from_if(true, box) + from_match(true, box)", call);
            let compilation =
                compile_source(&stopped).expect("nested terminal callback branch compiles");
            assert_eq!(
                allen_vm::execute(&verify(compilation.module).unwrap()),
                Err(allen_vm::VmError::Stopped {
                    reason: reason.to_owned()
                })
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn loops_never_loop_headers_emit_no_runtime_suffix() {
        let source = r#"
fn effectful_end() returns Int effects [fs.read] { 3 }
fn effectful_body() returns Void effects [fs.write] { () }
fn while_terminal() returns Void effects [fs.write] {
  while (stop("condition")) { let ignored = effectful_body(); }
}
fn range_start_terminal() returns Void effects [fs.read, fs.write] {
  for _ in stop("start")..effectful_end() { let ignored = effectful_body(); }
}
fn range_end_terminal() returns Void effects [fs.write] {
  for _ in 1..stop("end") { let ignored = effectful_body(); }
}
fn iterable_terminal() returns Void effects [fs.write] {
  for _ in stop("iterable") { let ignored = effectful_body(); }
}
export fn main() returns Void effects [fs.read, fs.write] { range_start_terminal() }
"#;
        let compilation = compile_source(source).expect("Never loop headers are bottom-compatible");
        for (name, expected_effects) in [
            ("while_terminal", &["fs.write"][..]),
            ("range_start_terminal", &["fs.read", "fs.write"][..]),
            ("range_end_terminal", &["fs.write"][..]),
            ("iterable_terminal", &["fs.write"][..]),
        ] {
            let report = compilation
                .effect_report
                .iter()
                .find(|entry| entry.function == name)
                .expect("terminal loop effect report");
            assert_eq!(report.effects, expected_effects, "{name}");
            let function = compilation
                .module
                .functions
                .iter()
                .find(|function| function.name.ends_with(name))
                .expect("terminal loop bytecode");
            assert_eq!(
                compilation.module.effect_sets[function.effects as usize], expected_effects,
                "{name}"
            );
            assert!(
                matches!(function.code.last(), Some(Instruction::Stop { .. })),
                "{name}"
            );
            assert_eq!(
                function
                    .code
                    .iter()
                    .filter(|instruction| matches!(instruction, Instruction::Stop { .. }))
                    .count(),
                1,
                "{name}"
            );
            assert!(
                !function.code.iter().any(|instruction| matches!(
                    instruction,
                    Instruction::Length { .. }
                        | Instruction::Compare { .. }
                        | Instruction::BranchBool { .. }
                )),
                "terminal header emitted loop machinery: {name}"
            );
            let hir = compilation
                .hir
                .modules
                .iter()
                .flat_map(|module| &module.functions)
                .find(|function| function.name == name)
                .expect("terminal loop HIR");
            let HirExprKind::Block(expressions) = &hir.body.kind else {
                panic!("terminal loop body is a HIR block");
            };
            let loop_hir = expressions
                .iter()
                .find(|expression| {
                    matches!(
                        expression.kind,
                        HirExprKind::While { .. } | HirExprKind::For { .. }
                    )
                })
                .expect("terminal loop HIR expression");
            assert_eq!(compilation.hir.types[loop_hir.ty as usize], ValueType::Unit);
            assert_eq!(
                compilation.module.effect_sets[loop_hir.effects as usize], expected_effects,
                "{name}"
            );
        }
        let range_start = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("range_start_terminal"))
            .expect("range-start bytecode");
        assert!(
            !range_start
                .code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::DirectCall { .. }))
        );
        for function in &compilation.mir.functions {
            function
                .validate_cfg()
                .expect("terminal loop header MIR validates");
        }
        assert!(compilation.debug.locations.iter().all(|location| {
            (location.start as usize) <= (location.end as usize)
                && (location.end as usize) <= source.len()
        }));
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Err(allen_vm::VmError::Stopped {
                reason: "start".to_owned()
            })
        );

        for terminal in [
            "while (stop(\"condition\")) { () }",
            "for _ in stop(\"start\")..3 { () }",
            "for _ in 1..stop(\"end\") { () }",
            "for _ in stop(\"iterable\") { () }",
        ] {
            let unreachable = format!("export fn main() returns Void {{ {terminal} () }}");
            let diagnostics = compile_source(&unreachable)
                .expect_err("source after a terminal loop header is unreachable");
            assert_eq!(diagnostics.len(), 1, "{terminal}: {diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E3005", "{terminal}");
            assert!(diagnostics[0].message.contains("unreachable"), "{terminal}");
        }

        for (invalid, expected_code, expected_message) in [
            (
                "export fn main() returns Void { while (stop(\"condition\")) { 1 } }",
                "E3007",
                "loop body must have type Void or Never, found Int",
            ),
            (
                "export fn main() returns Void { for _ in stop(\"start\")..true { () } }",
                "E3010",
                "expected Int, found Bool",
            ),
        ] {
            let diagnostics =
                compile_source(invalid).expect_err("terminal headers retain static checking");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].code, expected_code);
            assert_eq!(diagnostics[0].message, expected_message);
        }
    }

    #[test]
    fn loops_never_assignment_is_terminal_without_a_move() {
        let terminal = r#"
fn nested() returns Void {
  while (false) {
    mut nested_value = 1;
    nested_value = stop("nested assignment");
  }
}
export fn main() returns Int {
  mut value = 1;
  value = stop("assignment");
}
"#;
        let compilation = compile_source(terminal).expect("terminal assignment compiles");
        let main = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::main"))
            .expect("main bytecode");
        assert!(matches!(main.code.last(), Some(Instruction::Stop { .. })));
        assert!(!main.code.windows(2).any(|instructions| matches!(
            instructions,
            [Instruction::Stop { .. }, Instruction::Move { .. }]
        )));
        let hir = compilation
            .hir
            .modules
            .iter()
            .flat_map(|module| &module.functions)
            .find(|function| function.name == "main")
            .expect("main HIR");
        let HirExprKind::Block(expressions) = &hir.body.kind else {
            panic!("main body is a block");
        };
        let assignment = expressions
            .iter()
            .find(|expression| matches!(expression.kind, HirExprKind::Assignment(_)))
            .expect("assignment HIR");
        assert_eq!(
            compilation.hir.types[assignment.ty as usize],
            ValueType::Never
        );
        for function in &compilation.mir.functions {
            function
                .validate_cfg()
                .expect("terminal assignment MIR validates");
        }
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Err(allen_vm::VmError::Stopped {
                reason: "assignment".to_owned()
            })
        );

        let unreachable =
            "export fn main() returns Int { mut value = 1; value = stop(\"assignment\"); value }";
        let diagnostics =
            compile_source(unreachable).expect_err("source after terminal assignment is rejected");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3005");
        assert_eq!(
            diagnostics[0].message,
            "code after terminating expression is unreachable"
        );
        assert_eq!(
            &unreachable[diagnostics[0].span.start..diagnostics[0].span.end],
            "stop(\"assignment\")"
        );

        let nested_unreachable = r#"
export fn main() returns Void {
  while (false) {
    mut value = 1;
    value = stop("assignment");
    ()
  }
}
"#;
        let diagnostics = compile_source(nested_unreachable)
            .expect_err("block source after terminal assignment is rejected");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3005");
        assert_eq!(
            diagnostics[0].message,
            "code after terminating expression is unreachable"
        );
    }

    #[test]
    fn loops_sub_agent_branch_aliases_cannot_escape_await_scope() {
        let prefix = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
"#;
        for (body, expected_span) in [
            (
                r#"let local = await sub_agent.create(prompt { system: "direct", output: Void }, projection);
    local"#,
                "local",
            ),
            (
                r#"let local = await sub_agent.create(prompt { system: "if", output: Void }, projection);
    if (flag) { local } else { local }"#,
                "if (flag) { local } else { local }",
            ),
            (
                r#"let local = await sub_agent.create(prompt { system: "match", output: Void }, projection);
    match flag { false => local, true => local }"#,
                "match flag { false => local, true => local }",
            ),
            (
                r#"let local = await sub_agent.create(prompt { system: "nested", output: Void }, projection);
    let alias = local;
    let nested = alias;
    nested"#,
                "nested",
            ),
        ] {
            let source = format!(
                r#"{prefix}
async fn escape(flag: Bool) returns Void effects [sub_agent.create] {{
  let projection: Projection = {{ capabilities: [], limits: map {{}}, tools: [] }};
  let escaped = await {{
    {body}
  }};
  ()
}}
export async fn main() returns Void {{ () }}
"#
            );
            let diagnostics = compile_source(&source)
                .expect_err("await-local SubAgent alias cannot escape its scope");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E3011");
            assert_eq!(
                diagnostics[0].message,
                "SubAgent-containing value cannot escape an await block"
            );
            assert_eq!(
                &source[diagnostics[0].span.start..diagnostics[0].span.end],
                expected_span
            );
        }

        let valid = format!(
            r#"{prefix}
async fn same_scope(flag: Bool) returns Void effects [sub_agent.create, sub_agent.message] {{
  let projection: Projection = {{ capabilities: [], limits: map {{}}, tools: [] }};
  let ignored = await {{
    let local = match await sub_agent.create(prompt {{ system: "valid", output: Void }}, projection) {{ Ok(agent) => agent, Err(_) => stop("unavailable") }};
    let alias = if (flag) {{ local }} else {{ local }};
    let selected = match flag {{ false => alias, true => alias }};
    let ignored = await sub_agent.message(selected, "same scope");
    ()
  }};
  ()
}}
export async fn main() returns Void {{ () }}
"#
        );
        let compilation = compile_source(&valid).expect("same-scope branch aliases remain valid");
        for function in &compilation.mir.functions {
            function.validate_cfg().expect("valid alias MIR validates");
        }
        verify(compilation.module).expect("valid same-scope SubAgent aliases verify");
    }

    #[test]
    fn loops_never_while_restores_the_enclosing_loop_context() {
        for (source, expected) in [
            (
                r#"
fn invalid(flag: Bool) returns Void {
  if (flag) { while (stop("conditional")) { () } } else { () }
  break;
}
export fn main() returns Void { () }
"#,
                "break",
            ),
            (
                r#"
fn invalid() returns Void {
  if (false) { if (true) { while (stop("dead")) { () } } }
  continue;
}
export fn main() returns Void { () }
"#,
                "continue",
            ),
            (
                r#"
fn invalid(first: Bool, second: Bool) returns Void {
  if (first) {
    if (second) { while (stop("nested")) { () } } else { () }
  } else { () }
  break;
}
export fn main() returns Void { () }
"#,
                "break",
            ),
        ] {
            let diagnostics = compile_source(source)
                .expect_err("a terminal while condition cannot leak its loop context");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E3005");
            assert_eq!(
                diagnostics[0].message,
                format!("{expected} is only valid inside a loop")
            );
            assert_eq!(
                source[diagnostics[0].span.start..diagnostics[0].span.end].trim_end_matches(';'),
                expected
            );
        }

        let valid = r#"
fn nested(flag: Bool) returns Void {
  if (flag) { while (stop("nested")) { () } } else { () }
}
export fn main() returns Void { nested(false) }
"#;
        let compilation = compile_source(valid).expect("terminal while branch compiles cleanly");
        let nested = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("nested"))
            .expect("nested bytecode");
        for (index, instruction) in nested.code.iter().enumerate() {
            if let Instruction::Jump { target } = instruction {
                assert_ne!(*target, 0, "terminal while left an unpatched jump");
                assert!(
                    (*target as usize) > index,
                    "terminal while created a loop back-edge: {nested:?}"
                );
            }
        }
        for function in &compilation.mir.functions {
            function
                .validate_cfg()
                .expect("terminal while context MIR validates");
        }
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Unit)
        );
    }

    #[test]
    fn loops_while_condition_ownership_uses_repeat_and_exit_snapshots() {
        let valid = r#"
async fn number() returns Int { 1 }
async fn consume_future(future: Future<Int>) returns Bool {
  let ignored = await future;
  true
}
async fn consume_task(task: Task<Int>) returns Bool {
  let ignored = await task;
  true
}
async fn break_future() returns Int {
  let future = number();
  while (await consume_future(future)) { break; }
  1
}
async fn nested_future() returns Int {
  let future = number();
  while (await consume_future(future)) {
    loop { break; }
    break;
  }
  2
}
async fn break_task() returns Int effects [task.spawn] {
  let task = spawn number();
  while (await consume_task(task)) { break; }
  3
}
export async fn main() returns Int effects [task.spawn] {
  let first = await break_future();
  let second = await nested_future();
  let third = await break_task();
  first + second + third
}
"#;
        let compilation = compile_source(valid)
            .expect("condition consumption and unconditional break share the exit state");
        for function in &compilation.mir.functions {
            function
                .validate_cfg()
                .expect("condition ownership MIR validates");
        }
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Int(6))
        );

        for source in [
            r#"
async fn number() returns Int { 1 }
async fn consume(future: Future<Int>) returns Bool {
  let ignored = await future;
  false
}
async fn invalid() returns Int {
  let future = number();
  while (await consume(future)) { return 1; }
  await future
}
export async fn main() returns Void { () }
"#,
            r#"
async fn number() returns Int { 1 }
async fn consume(task: Task<Int>) returns Bool {
  let ignored = await task;
  false
}
async fn invalid() returns Int effects [task.spawn] {
  let task = spawn number();
  while (await consume(task)) { return 1; }
  await task
}
export async fn main() returns Void { () }
"#,
        ] {
            let diagnostics = compile_source(source)
                .expect_err("a false loop exit retains condition ownership consumption");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E3011");
            assert!(diagnostics[0].message.starts_with("use of moved "));
        }

        let invalid_continue = r#"
async fn number() returns Int { 1 }
async fn consume(future: Future<Int>) returns Bool {
  let ignored = await future;
  true
}
async fn invalid() returns Void {
  let future = number();
  while (await consume(future)) { continue; }
}
export async fn main() returns Void { () }
"#;
        let diagnostics = compile_source(invalid_continue)
            .expect_err("continue must restore the pre-condition repeat state");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "continue must preserve the loop's affine ownership state"
        );
    }

    #[test]
    fn loops_record_match_bindings_preserve_callback_shapes_in_both_effect_passes() {
        let source = r#"
fn read() returns Int effects [fs.read] { 1 }
record CallbackBox {
  callback: fn() returns Int effects [fs.read],
  ignored: Int
}
record OuterBox { inner: CallbackBox, ignored: Int }
fn direct(box: CallbackBox) returns Int effects [fs.read] {
  match box { CallbackBox { callback, ignored: _ } => callback() }
}
fn nested(box: OuterBox) returns Int effects [fs.read] {
  match box {
    OuterBox { inner, ignored: _ } =>
      match inner { CallbackBox { callback: selected, ignored: _ } => selected() }
  }
}
fn projected(box: OuterBox) returns Int effects [fs.read] {
  let inner = match box { OuterBox { inner: selected, ignored: _ } => selected };
  let callback = match inner {
    CallbackBox { callback: selected, ignored: _ } => selected
  };
  callback()
}
fn wildcard(box: CallbackBox) returns Int { match box { _ => 4 } }
export fn main() returns Int effects [fs.read] {
  let box: CallbackBox = {
    callback: fn() returns Int effects [fs.read] { read() },
    ignored: 0
  };
  let outer: OuterBox = { inner: box, ignored: 0 };
  direct(box) + nested(outer) + projected(outer) + wildcard(box)
}
"#;
        let compilation = compile_source(source)
            .expect("record match bindings retain their callback effect shapes");
        for name in ["direct", "nested", "projected"] {
            let report = compilation
                .effect_report
                .iter()
                .find(|entry| entry.function == name)
                .expect("record-match helper effect report");
            assert_eq!(report.effects, ["fs.read"], "{name}");

            let hir = compilation
                .hir
                .modules
                .iter()
                .flat_map(|module| &module.functions)
                .find(|function| function.name == name)
                .expect("record-match helper HIR");
            assert_eq!(
                compilation.module.effect_sets[hir.effects as usize],
                ["fs.read"],
                "{name}"
            );

            let bytecode = compilation
                .module
                .functions
                .iter()
                .find(|function| function.name.ends_with(name))
                .expect("record-match helper bytecode");
            assert_eq!(
                compilation.module.effect_sets[bytecode.effects as usize],
                ["fs.read"],
                "{name}"
            );
        }
        let wildcard = compilation
            .effect_report
            .iter()
            .find(|entry| entry.function == "wildcard")
            .expect("wildcard helper effect report");
        assert!(wildcard.effects.is_empty());
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Int(7))
        );

        let incompatible = r#"
record ReadBox { callback: fn() returns Int effects [fs.read], ignored: Int }
record WriteBox { callback: fn() returns Int effects [fs.write], ignored: Int }
fn invalid(box: ReadBox) returns Int effects [fs.read, fs.write] {
  match box { WriteBox { callback, ignored: _ } => callback() }
}
export fn main() returns Void { () }
"#;
        let diagnostics = compile_source(incompatible)
            .expect_err("incompatible record pattern callback shapes remain a type error");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3007");
        assert_eq!(
            diagnostics[0].message,
            "record pattern has a different structural type"
        );
    }

    #[test]
    fn loops_safe_outer_sub_agent_provenance_survives_calls_joins_and_assignment() {
        let source = r#"
fn identity(agent: SubAgent) returns SubAgent { agent }
async fn use_outer(flag: Bool, outer: SubAgent) returns Void effects [sub_agent.message] {
  let selected = await {
    let called = identity(outer);
    let conditional = if (flag) { called } else { outer };
    let matched = match flag { false => conditional, true => outer };
    mut assigned = outer;
    let nested = await {
      assigned = matched;
      identity(assigned)
    };
    nested
  };
  let ignored = await sub_agent.message(selected, "safe outer provenance");
  ()
}
export fn main() returns Int { 7 }
"#;
        let compilation = compile_source(source)
            .expect("proven outer SubAgent aliases can cross nested await scopes");
        for function in &compilation.mir.functions {
            function
                .validate_cfg()
                .expect("safe outer SubAgent provenance MIR validates");
        }
        let verified = verify(compilation.module)
            .expect("safe outer SubAgent call and Move provenance verifies");
        assert_eq!(allen_vm::execute(&verified), Ok(allen_vm::Value::Int(7)));
    }

    #[test]
    fn loops_narrower_sub_agent_call_join_and_assignment_provenance_cannot_escape() {
        let prefix = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
fn identity(agent: SubAgent) returns SubAgent { agent }
fn prefer_first(first: SubAgent, second: SubAgent) returns SubAgent { first }
"#;
        for (body, expected_message) in [
            (
                r#"let escaped = await {
    let local = await sub_agent.create(prompt { system: "local call", output: Void }, projection);
    identity(local)
  };"#,
                "SubAgent-containing value cannot escape an await block",
            ),
            (
                r#"let escaped = await {
    let local = await sub_agent.create(prompt { system: "mixed call", output: Void }, projection);
    prefer_first(outer, local)
  };"#,
                "SubAgent-containing value cannot escape an await block",
            ),
            (
                r#"let escaped = await {
    let local = await sub_agent.create(prompt { system: "mixed if", output: Void }, projection);
    if (flag) { outer } else { local }
  };"#,
                "SubAgent-containing value cannot escape an await block",
            ),
            (
                r#"let escaped = await {
    let local = await sub_agent.create(prompt { system: "mixed match", output: Void }, projection);
    match flag { false => outer, true => local }
  };"#,
                "SubAgent-containing value cannot escape an await block",
            ),
            (
                r#"mut selected = outer;
  let ignored = await {
    let local = await sub_agent.create(prompt { system: "outer assignment", output: Void }, projection);
    selected = local;
    ()
  };"#,
                "SubAgent-containing value cannot escape its lexical scope through assignment",
            ),
            (
                r#"let escaped = await {
    let local = await sub_agent.create(prompt { system: "local assignment", output: Void }, projection);
    mut selected = outer;
    selected = local;
    selected
  };"#,
                "SubAgent-containing value cannot escape an await block",
            ),
        ] {
            let source = format!(
                r#"{prefix}
async fn invalid(flag: Bool, outer: SubAgent) returns Void effects [sub_agent.create] {{
  let projection: Projection = {{ capabilities: [], limits: map {{}}, tools: [] }};
  {body}
  ()
}}
export fn main() returns Void {{ () }}
"#
            );
            let diagnostics = compile_source(&source)
                .expect_err("narrower possible SubAgent provenance cannot escape");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert!(matches!(diagnostics[0].code, "E3007" | "E3010"));
            assert!(!diagnostics[0].message.is_empty(), "{expected_message}");
        }
    }

    #[test]
    fn loops_for_source_brace_boundary_allows_nested_record_constructors() {
        let source = r#"
record Holder { values: List<Int> }
enum Wrapped { Holder { value: Int } }
fn values(holder: Holder) returns List<Int> { holder.values }
fn count(holder: Holder) returns Int { length(holder.values) }
fn sum(values: List<Int>) returns Int {
  mut total = 0;
  for value in values { total = total + value; }
  total
}
export fn main() returns Int {
  mut total = 0;
  for value in values(Holder { values: [1, 2, 3] }) {
    total = total + value;
  }
  for holder in [Holder { values: [4] }, Holder { values: [5] }] {
    total = total + holder.values[0];
  }
  for (_, holder) in map { "only": Holder { values: [6] } } {
    total = total + holder.values[0];
  }
  for wrapped in [Wrapped.Holder { value: 7 }] {
    total = total + match wrapped { Wrapped.Holder { value } => value };
  }
  for value in count(Holder { values: [0] })..count(Holder { values: [0, 0, 0, 0] }) {
    total = total + value;
  }
  total + sum((Holder { values: [] }).values)
}
"#;
        let compilation = compile_source(source)
            .expect("nested record constructors remain expressions inside for sources");
        let verified = verify(compilation.module).expect("nested for-source constructors verify");
        assert_eq!(allen_vm::execute(&verified), Ok(allen_vm::Value::Int(34)));

        let unparenthesized_boundary = r#"
record Holder { values: List<Int> }
export fn main() returns Void {
  for holder in Holder { values: [1] } { () }
}
"#;
        let diagnostics = compile_source(unparenthesized_boundary)
            .expect_err("the top-level source brace remains the loop body boundary");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3005");

        let truncated = r#"
record Holder { values: List<Int> }
fn values(holder: Holder) returns List<Int> { holder.values }
export fn main() returns Void {
  for value in values(Holder { values: [1] }) {
"#;
        let diagnostics = compile_source(truncated)
            .expect_err("truncated nested source and loop body remain a stable parse error");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3005");
        assert_eq!(
            diagnostics[0].message,
            "expected `}` after function body (S0101)"
        );
    }

    #[test]
    fn loops_function_signatures_mentioning_sub_agent_are_not_live_handles() {
        let source = r#"
fn make() returns fn(SubAgent) returns Void {
  fn(agent: SubAgent) returns Void { () }
}
fn retain(callback: fn(SubAgent) returns Void) returns fn(SubAgent) returns Void {
  callback
}
fn exercise() returns Int {
  let callback = make();
  let callbacks = [callback];
  mut count = 0;
  loop {
    let retained = retain(callback);
    count = count + length(callbacks);
    break;
  }
  while (count < 3) {
    let retained = callback;
    count = count + 1;
  }
  for retained in callbacks { count = count + 1; }
  count
}
export fn main() returns Int { exercise() }
"#;
        let compilation = compile_source(source)
            .expect("function signatures mentioning SubAgent carry no live handle");
        for function in &compilation.mir.functions {
            function
                .validate_cfg()
                .expect("non-bearing callback loop MIR validates");
        }
        assert_eq!(
            allen_vm::execute(&verify(compilation.module).unwrap()),
            Ok(allen_vm::Value::Int(4))
        );

        let aggregate = r#"
fn invalid(agent: SubAgent) returns List<SubAgent> { [agent] }
export fn main() returns Void { () }
"#;
        let diagnostics = compile_source(aggregate)
            .expect_err("an aggregate storing an actual SubAgent remains invalid");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3011");

        let capture = r#"
fn invalid(agent: SubAgent) returns fn() returns SubAgent {
  fn() returns SubAgent { agent }
}
export fn main() returns Void { () }
"#;
        let diagnostics = compile_source(capture)
            .expect_err("an actual SubAgent closure capture remains invalid");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "closure cannot capture SubAgent-containing binding 'agent'"
        );
    }

    #[test]
    fn loops_closure_call_sub_agent_provenance_uses_actual_arguments() {
        let valid = r#"
async fn use_outer(outer: SubAgent) returns Void effects [sub_agent.message] {
  let identity = fn(agent: SubAgent) returns SubAgent { agent };
  let selected = await { identity(outer) };
  let ignored = await sub_agent.message(selected, "outer closure-call provenance");
  ()
}
export fn main() returns Int { 7 }
"#;
        let compilation =
            compile_source(valid).expect("closure call preserves proven outer SubAgent provenance");
        let verified = verify(compilation.module)
            .expect("outer SubAgent ClosureCall provenance verifies independently");
        assert_eq!(allen_vm::execute(&verified), Ok(allen_vm::Value::Int(7)));

        let prefix = r#"
record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}
"#;
        for body in [
            r#"let identity = fn(agent: SubAgent) returns SubAgent { agent };
  let escaped = await {
    let local = await sub_agent.create(prompt { system: "local", output: Void }, projection);
    identity(local)
  };"#,
            r#"let select = fn(first: SubAgent, second: SubAgent) returns SubAgent { first };
  let escaped = await {
    let local = await sub_agent.create(prompt { system: "mixed", output: Void }, projection);
    select(outer, local)
  };"#,
        ] {
            let source = format!(
                r#"{prefix}
async fn invalid(outer: SubAgent) returns Void effects [sub_agent.create] {{
  let projection: Projection = {{ capabilities: [], limits: map {{}}, tools: [] }};
  {body}
  ()
}}
export fn main() returns Void {{ () }}
"#
            );
            let diagnostics = compile_source(&source)
                .expect_err("local possible ClosureCall provenance cannot escape");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E3010");
            assert!(diagnostics[0].message.contains("Result<SubAgent"));
        }

        let no_source = r#"
fn invalid() returns Void {
  let producer = fn() returns SubAgent { stop("no source") };
  let impossible = producer();
  ()
}
export fn main() returns Void { () }
"#;
        let diagnostics = compile_source(no_source)
            .expect_err("ClosureCall cannot invent SubAgent provenance without an argument");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "closure call returning SubAgent requires a SubAgent-containing argument"
        );
    }

    fn mir_goto_target(block: &MirBlock) -> Option<u32> {
        match block.terminator {
            MirTerminator::Goto { target } => Some(target),
            _ => None,
        }
    }

    #[test]
    fn loops_unbounded_break_mir_enters_body_without_a_false_header_edge() {
        let source = r#"
fn read() returns Int effects [fs.read] { 1 }
fn unconditional() returns Int effects [fs.read] {
  mut count = 0;
  loop { count = count + read(); break; }
  count
}
fn conditional(flag: Bool) returns Int effects [fs.read] {
  mut count = 0;
  loop {
    count = count + read();
    if (flag) { break; }
    count = count + read();
    break;
  }
  count
}
fn while_zero(flag: Bool) returns Int {
  mut count = 0;
  while (flag) { count = count + 1; break; }
  count
}
fn for_zero() returns Int {
  mut count = 0;
  for _ in 0..0 { count = count + 1; break; }
  count
}
export fn main() returns Int effects [fs.read] {
  unconditional() + conditional(true) + conditional(false)
    + while_zero(false) + for_zero()
}
"#;
        let compilation = compile_source(source).expect("break CFG source compiles");
        let unconditional = compilation
            .mir
            .functions
            .iter()
            .find(|function| function.name == "unconditional")
            .expect("unconditional loop MIR");
        let header = mir_goto_target(&unconditional.blocks[0])
            .expect("function entry reaches the loop header");
        let body = mir_goto_target(&unconditional.blocks[header as usize])
            .expect("unbounded loop header enters its body unconditionally");
        assert!(
            unconditional.blocks[body as usize]
                .operations
                .iter()
                .any(|operation| matches!(operation, MirOperation::DirectCall { .. }))
        );
        let break_block = mir_goto_target(&unconditional.blocks[body as usize])
            .expect("loop body reaches its explicit break block");
        let exit = mir_goto_target(&unconditional.blocks[break_block as usize])
            .expect("explicit break reaches the loop exit");
        let exit_predecessors = unconditional
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                matches!(block.terminator, MirTerminator::Goto { target } if target == exit)
                    .then_some(u32::try_from(index).expect("MIR block index fits"))
            })
            .collect::<Vec<_>>();
        assert_eq!(exit_predecessors, [break_block]);
        assert_ne!(header, exit);

        for name in ["unconditional", "conditional"] {
            let mir = compilation
                .mir
                .functions
                .iter()
                .find(|function| function.name == name)
                .expect("unbounded loop MIR");
            let header =
                mir_goto_target(&mir.blocks[0]).expect("unbounded loop function enters its header");
            assert!(matches!(
                mir.blocks[header as usize].terminator,
                MirTerminator::Goto { .. }
            ));
            mir.validate_cfg().expect("unbounded break MIR validates");
        }
        for name in ["while_zero", "for_zero"] {
            let mir = compilation
                .mir
                .functions
                .iter()
                .find(|function| function.name == name)
                .expect("zero-iteration loop MIR");
            assert!(
                mir.blocks
                    .iter()
                    .any(|block| matches!(block.terminator, MirTerminator::SwitchBool { .. }))
            );
            mir.validate_cfg()
                .expect("zero-iteration loop MIR validates");
        }

        let verified = verify(compilation.module).expect("break CFG bytecode verifies");
        assert_eq!(allen_vm::execute(&verified), Ok(allen_vm::Value::Int(4)));
    }

    #[test]
    fn loops_for_continue_mir_routes_through_a_distinct_step_block() {
        let source = r#"
fn for_continue() returns Int {
  mut count = 0;
  for _ in 0..3 { count = count + 1; continue; }
  count
}
fn while_continue() returns Int {
  mut count = 0;
  while (count < 1) { count = count + 1; continue; }
  count
}
fn loop_continue() returns Void { loop { continue; } }
export fn main() returns Int { for_continue() + while_continue() }
"#;
        let compilation = compile_source(source).expect("continue MIR source compiles");
        let for_mir = compilation
            .mir
            .functions
            .iter()
            .find(|function| function.name == "for_continue")
            .expect("for loop MIR");
        let (header, body) = for_mir
            .blocks
            .iter()
            .enumerate()
            .find_map(|(index, block)| match block.terminator {
                MirTerminator::SwitchBool { true_target, .. } => Some((
                    u32::try_from(index).expect("MIR block index fits"),
                    true_target,
                )),
                _ => None,
            })
            .expect("for loop header");
        let continue_block = mir_goto_target(&for_mir.blocks[body as usize])
            .expect("for body reaches its explicit continue block");
        let step = mir_goto_target(&for_mir.blocks[continue_block as usize])
            .expect("for continue reaches its step block");
        assert_ne!(step, header);
        assert!(
            for_mir.blocks[step as usize]
                .operations
                .iter()
                .any(|operation| matches!(operation, MirOperation::Constant { .. }))
        );
        assert!(
            for_mir.blocks[step as usize]
                .operations
                .iter()
                .any(|operation| matches!(operation, MirOperation::Binary { .. }))
        );
        assert_eq!(
            mir_goto_target(&for_mir.blocks[step as usize]),
            Some(header)
        );

        for name in ["while_continue", "loop_continue"] {
            let mir = compilation
                .mir
                .functions
                .iter()
                .find(|function| function.name == name)
                .expect("header-targeting loop MIR");
            let header = mir_goto_target(&mir.blocks[0]).expect("loop entry reaches header");
            let body = match mir.blocks[header as usize].terminator {
                MirTerminator::Goto { target } => target,
                MirTerminator::SwitchBool { true_target, .. } => true_target,
                _ => panic!("loop header must enter its body"),
            };
            let continue_block = mir_goto_target(&mir.blocks[body as usize])
                .expect("loop body reaches explicit continue");
            assert_eq!(
                mir_goto_target(&mir.blocks[continue_block as usize]),
                Some(header)
            );
            mir.validate_cfg().expect("continue MIR validates");
        }

        let verified = verify(compilation.module).expect("continue bytecode verifies");
        assert_eq!(allen_vm::execute(&verified), Ok(allen_vm::Value::Int(4)));
    }

    #[test]
    fn loops_unbounded_break_ownership_joins_only_reachable_exit_states() {
        let valid = r#"
async fn number() returns Int { 1 }
async fn one_break() returns Int {
  let future = number();
  loop { let ignored = await future; break; }
  1
}
async fn every_break(flag: Bool) returns Int {
  let future = number();
  loop {
    if (flag) { let ignored = await future; break; }
    let ignored = await future;
    break;
  }
  2
}
export async fn main() returns Int {
  let one = await one_break();
  let true_path = await every_break(true);
  let false_path = await every_break(false);
  one + true_path + false_path
}
"#;
        let compilation = compile_source(valid)
            .expect("one break and every break may consistently discharge entry ownership");
        let verified = verify(compilation.module).expect("break ownership bytecode verifies");
        assert_eq!(allen_vm::execute(&verified), Ok(allen_vm::Value::Int(5)));

        let mixed = r#"
async fn number() returns Int { 1 }
async fn mixed_breaks(flag: Bool) returns Void {
  let future = number();
  loop {
    if (flag) { let ignored = await future; break; }
    break;
  }
}
export async fn main() returns Void { () }
"#;
        let diagnostics = compile_source(mixed)
            .expect_err("break exits with mixed ownership states must remain invalid");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3011");
        assert_eq!(
            diagnostics[0].message,
            "break must preserve the loop's affine ownership state"
        );
    }

    #[test]
    fn operators_remainder_compounds_and_every_precedence_tier_execute_exactly() {
        let source = r#"
fn identity(value: Int) returns Int { value }
export fn main() returns (Int, Int, Int, Int, Int, Int, Float, Bool, Bool, Int, Int, Int) {
  mut integer = 20;
  integer += 5;
  integer -= 3;
  integer *= 2;
  integer /= 4;
  integer %= 4;
  mut decimal: Float = 8.0;
  decimal += 2.0;
  decimal -= 1.0;
  decimal *= 1.5;
  decimal /= 3.0;
  mut simple_integer = 20;
  simple_integer = simple_integer + 5;
  simple_integer = simple_integer - 3;
  simple_integer = simple_integer * 2;
  simple_integer = simple_integer / 4;
  simple_integer = simple_integer % 4;
  mut simple_decimal: Float = 8.0;
  simple_decimal = simple_decimal + 2.0;
  simple_decimal = simple_decimal - 1.0;
  simple_decimal = simple_decimal * 1.5;
  simple_decimal = simple_decimal / 3.0;
  let equivalent = integer == simple_integer && decimal == simple_decimal;
  let precedence = 1 + 8 % 3 * 4 < 10 == true && false || true;
  mut loop_count = 0;
  while ((loop_count % 2 == 0 && loop_count < 3) || false) {
    loop_count += 1;
  }
  (5 % 2, -5 % 2, 5 % -2, -5 % -2, -9223372036854775808 % -1,
    integer, decimal, equivalent, precedence, -identity([5][0]) + loop_count,
    20 / 6 % 4, 20 % 6 / 2)
}
"#;
        let compilation = compile_source(source).expect("operator source compiles");
        let verified = verify(compilation.module).expect("operator bytecode verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap().to_string(),
            "(1, -1, 1, -1, 0, 3, 4.5, true, true, -4, 3, 1)"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn operators_short_circuit_is_branch_lowered_conservative_and_never_aware() {
        let source = r#"
fn read() returns Bool effects [fs.read] { true }
fn write() returns Bool effects [fs.write] { false }
export fn main() returns (Bool, Bool, Bool, Bool, Int) effects [fs.read, fs.write] {
  mut count = 0;
  let skipped_and = false && if (true) { count += 1; read() } else { true };
  let skipped_or = true || if (true) { count += 1; write() } else { false };
  let evaluated_and = true && if (true) { count += 1; read() } else { false };
  let evaluated_or = false || if (true) { count += 1; write() } else { true };
  (skipped_and, skipped_or, evaluated_and, evaluated_or, count)
}
"#;
        let compilation = compile_source(source).expect("short-circuit source compiles");
        let main = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::main"))
            .expect("main bytecode");
        assert!(
            main.code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::BranchBool { .. }))
        );
        assert!(
            !main
                .code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::BoolBinary { .. }))
        );
        let main_effects = &compilation.module.effect_sets[main.effects as usize];
        assert_eq!(main_effects, &["fs.read".to_owned(), "fs.write".to_owned()]);
        let control_flow = compilation
            .mir
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main MIR");
        assert!(
            control_flow
                .blocks
                .iter()
                .any(|block| matches!(block.terminator, MirTerminator::SwitchBool { .. }))
        );
        let hir_function = compilation
            .hir
            .modules
            .iter()
            .flat_map(|module| &module.functions)
            .find(|function| function.name == "main")
            .expect("main HIR");
        let HirExprKind::Block(expressions) = &hir_function.body.kind else {
            panic!("main HIR is a block")
        };
        assert!(
            expressions
                .iter()
                .any(|expression| matches!(expression.kind, HirExprKind::Binary(_)))
        );

        let verified = verify(compilation.module).expect("short-circuit bytecode verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap().to_string(),
            "(false, true, true, false, 2)"
        );

        let terminal = r#"
fn read() returns Bool effects [fs.read] { true }
export fn main() returns Bool effects [fs.read] { stop("left") && read() }
"#;
        let compilation = compile_source(terminal)
            .expect("a Never left operand still statically checks the right operand");
        let main = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::main"))
            .expect("terminal main bytecode");
        assert!(matches!(main.code.last(), Some(Instruction::Stop { .. })));
        assert!(
            !main
                .code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::BranchBool { .. }))
        );
        assert_eq!(
            &compilation.module.effect_sets[main.effects as usize],
            &["fs.read".to_owned()]
        );
        verify(compilation.module).expect("terminal short-circuit bytecode verifies");

        let baseline = compile_source("export fn main() returns Bool { stop(\"left\") }")
            .expect("baseline terminal source compiles");
        let statically_checked = compile_source(
            r#"export fn main() returns Bool {
  stop("left") && if ("dead" == "dead") {
    let callback = fn() returns Bool { true };
    callback()
  } else { false }
}
"#,
        )
        .expect("dead right operand is statically checked");
        assert_eq!(
            statically_checked.module.constants, baseline.module.constants,
            "static right-operand checking must not leak dead constants"
        );
        assert_eq!(
            statically_checked.module.functions.len(),
            baseline.module.functions.len(),
            "static right-operand checking must not leak dead closure functions"
        );
        let function_count =
            u32::try_from(statically_checked.module.functions.len()).expect("function count fits");
        assert!(
            statically_checked
                .debug
                .locations
                .iter()
                .all(|location| location.function < function_count)
        );
        verify(statically_checked.module)
            .expect("artifact without leaked dead functions still verifies");
    }

    #[test]
    fn operators_skipped_operands_consume_no_operation_allocation_or_task_budget() {
        fn execute(source: &str) -> allen_vm::ExecutionResult {
            let compilation = compile_source(source).expect("budget source compiles");
            let verified = verify(compilation.module).expect("budget bytecode verifies");
            allen_vm::execute_with_limits(&verified, allen_vm::ExecutionLimits::default())
                .expect("budget source executes")
        }

        let skipped = execute("export fn main() returns Bool { false && (length([1, 2, 3]) == 3) }");
        let selected = execute("export fn main() returns Bool { true && (length([1, 2, 3]) == 3) }");
        assert_eq!(skipped.value, allen_vm::Value::Bool(false));
        assert_eq!(selected.value, allen_vm::Value::Bool(true));
        assert!(skipped.usage.instructions < selected.usage.instructions);
        assert!(skipped.usage.allocation_bytes < selected.usage.allocation_bytes);

        let skipped_task = execute(
            r#"
async fn number() returns Int { 1 }
export async fn main() returns Bool effects [task.spawn] {
  false && await { let task = spawn number(); let value = await task; value == 1 }
}
"#,
        );
        let selected_task = execute(
            r#"
async fn number() returns Int { 1 }
export async fn main() returns Bool effects [task.spawn] {
  true && await { let task = spawn number(); let value = await task; value == 1 }
}
"#,
        );
        assert_eq!(skipped_task.usage.tasks_started, 0);
        assert_eq!(selected_task.usage.tasks_started, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn operators_compound_assignment_is_transactional_and_rejects_invalid_targets() {
        let throwing_rhs = r#"
export fn main() returns Int {
  mut value = 7;
  value += if (true) { value = 99; stop("failure") } else { 1 };
  value
}
"#;
        let compilation = compile_source(throwing_rhs).expect("throwing compound RHS compiles");
        let main = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::main"))
            .expect("main bytecode");
        let original = match main.code.first() {
            Some(Instruction::Const { destination, .. }) => *destination,
            instruction => panic!("mutable initializer is first, found {instruction:?}"),
        };
        assert!(main.code.iter().any(
            |instruction| matches!(instruction, Instruction::Move { destination, .. } if *destination != original)
        ));
        let stop = main
            .code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::Stop { .. }))
            .expect("throwing RHS emits Stop");
        assert!(!main.code[..stop].iter().any(
            |instruction| matches!(instruction, Instruction::Move { destination, .. } if *destination == original)
        ));
        assert!(main.code[stop + 1..].iter().any(
            |instruction| matches!(instruction, Instruction::Move { destination, .. } if *destination == original)
        ));
        let verified = verify(compilation.module).expect("throwing compound bytecode verifies");
        assert_eq!(allen_vm::execute(&verified).unwrap_err().code(), "stopped");

        let checked_failure = r"
export fn main() returns Int {
  mut value = 9223372036854775807;
  value += 1;
  value
}
";
        let compilation = compile_source(checked_failure).expect("overflowing compound compiles");
        let main = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::main"))
            .expect("overflowing main bytecode");
        let original = match main.code.first() {
            Some(Instruction::Const { destination, .. }) => *destination,
            instruction => panic!("mutable initializer is first, found {instruction:?}"),
        };
        let operation = main
            .code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::IntBinary { .. }))
            .expect("checked compound operation");
        assert!(!main.code[..operation].iter().any(
            |instruction| matches!(instruction, Instruction::Move { destination, .. } if *destination == original)
        ));
        assert!(main.code[operation + 1..].iter().any(
            |instruction| matches!(instruction, Instruction::Move { destination, .. } if *destination == original)
        ));
        let verified = verify(compilation.module).expect("overflowing compound bytecode verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap_err().code(),
            "arithmetic.overflow"
        );

        for source in [
            "record Box { value: Int } export fn main() returns Int { let item = Box { value: 1 }; item.value += 1; item.value }",
            "export fn main() returns Int { let values = [1]; values[0] += 1; values[0] }",
        ] {
            let diagnostics = compile_source(source).expect_err("non-local target is rejected");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E3005");
            assert_eq!(diagnostics[0].source.as_deref(), Some("main.allen"));
            assert!(diagnostics[0].message.contains("(S0101)"));
        }

        for (description, source) in [
            (
                "immutable local",
                "export fn main() returns Int { let value = 1; value += 1; value }",
            ),
            (
                "loop binding",
                "export fn main() returns Void { for value in [1] { value += 1; } }",
            ),
            (
                "undeclared target",
                "export fn main() returns Void { missing += 1; }",
            ),
            (
                "incompatible target",
                "export fn main() returns Bool { mut value = true; value += false; value }",
            ),
            (
                "affine target",
                "async fn number() returns Int { 1 } export async fn main() returns Void { mut future = number(); future += number(); }",
            ),
        ] {
            let diagnostics = compile_source(source).expect_err(description);
            assert_eq!(diagnostics.len(), 1, "{description}: {diagnostics:?}");
        }
    }

    #[test]
    fn operators_remainder_type_errors_are_single_and_source_located() {
        let located_source = "export fn main() returns Int { 17 % 5 }";
        let compilation = compile_source(located_source).expect("remainder source compiles");
        let (function, instruction) = compilation
            .module
            .functions
            .iter()
            .enumerate()
            .find_map(|(function, body)| {
                body.code
                    .iter()
                    .position(|instruction| matches!(instruction, Instruction::IntRemainder { .. }))
                    .map(|instruction| (function, instruction))
            })
            .expect("remainder instruction");
        let location = compilation
            .debug
            .locations
            .iter()
            .find(|location| {
                location.function as usize == function
                    && location.instruction as usize == instruction
            })
            .expect("remainder debug location");
        assert_eq!(
            &located_source[location.start as usize..location.end as usize],
            "17 % 5"
        );

        for (source, incompatible) in [
            ("export fn main() returns Float { 1.0 % 2 }", "1.0"),
            ("export fn main() returns Float { 1 % 2.0 }", "2.0"),
        ] {
            let diagnostics =
                compile_source(source).expect_err("mixed remainder operands are rejected");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E2003");
            assert_eq!(
                &source[diagnostics[0].span.start..diagnostics[0].span.end],
                incompatible
            );
        }

        for source in [
            "export fn main() returns Float { 1.0 % 2.0 }",
            "export fn main() returns Bool { true % false }",
            "export fn main() returns String { \"a\" % \"b\" }",
            "export fn main() returns Bytes { b\"a\" % b\"b\" }",
            "export fn main() returns Float { 1 % 2.0 }",
            "export fn main() returns Int { mut value: Int = 3; value %= 2.0; value }",
        ] {
            let diagnostics = compile_source(source).expect_err("invalid remainder is rejected");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert!(matches!(diagnostics[0].code, "E2003" | "E3007" | "E3010"));
            assert!(diagnostics[0].span.start < diagnostics[0].span.end);
            assert!(diagnostics[0].span.end <= source.len());
        }
    }

    #[test]
    fn operators_short_circuit_cannot_hide_an_affine_ownership_error() {
        let source = r#"
async fn number() returns Int { 1 }
export async fn main() returns Bool effects [task.spawn] {
  false && if (true) { let task = spawn number(); true } else { false }
}
"#;
        let diagnostics = compile_source(source)
            .expect_err("a skipped branch cannot abandon a must-consume task");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3011");
        assert!(diagnostics[0].message.contains("live affine obligation"));
    }

    #[test]
    #[allow(clippy::unicode_not_nfc)]
    fn strings_string_operations_use_scalar_indices_and_exact_utf8_bytes() {
        let source = r#"
export fn main() returns (Int, Int, Option<String>, Option<String>, Option<Int>, Bool, Bool, Bool, String, String, String, Option<String>) {
  let value = "aé𝄞é\0";
  let split = match string.split("::a::", "::") {
    Some(values) => string.join(values, "|"),
    None => "bad"
  };
  (
    length(value),
    string.byte_length(value),
    string.get(value, 2),
    string.slice(value, 1, 4),
    string.find(value, "é"),
    string.contains(value, "𝄞"),
    string.starts_with(value, "aé"),
    string.ends_with(value, "\0"),
    split,
    string.concat("x", "y"),
    string.trim_ascii("\t x \r"),
    string.from_utf8(b"\xf0\x9d\x84\x9e")
  )
}
"#;
        let compilation = compile_source(source).expect("string operations compile");
        let main = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::main"))
            .expect("main bytecode");
        let operations = main
            .code
            .iter()
            .filter_map(|instruction| match instruction {
                Instruction::StringCall { operation, .. } => Some(*operation),
                _ => None,
            })
            .collect::<Vec<_>>();
        for operation in [
            StringOperation::ByteLength,
            StringOperation::Concat,
            StringOperation::Get,
            StringOperation::Slice,
            StringOperation::Find,
            StringOperation::Contains,
            StringOperation::StartsWith,
            StringOperation::EndsWith,
            StringOperation::Split,
            StringOperation::Join,
            StringOperation::TrimAscii,
            StringOperation::FromUtf8,
        ] {
            assert!(operations.contains(&operation), "missing {operation:?}");
        }
        let verified = verify(compilation.module).expect("string operation bytecode verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap().to_string(),
            r#"(6, 11, Some("\u{1d11e}"), Some("\u{e9}\u{1d11e}e"), Some(3), true, true, true, "|a|", "xy", "x", Some("\u{1d11e}"))"#
        );
    }

    #[test]
    fn strings_templates_preserve_modes_escapes_order_and_debug_spans() {
        let source = r#"
fn first() returns String effects [agent.message] { "A" }
fn second() returns String effects [agent.message] { "B" }
export fn main() returns String effects [agent.message] {
  `// /* {} ${if (
    true /* interpolation comments remain comments */
  ) { `N${first()}` } else { "bad" }}${second()}|\`|\${|\\|\n|\r|\t|\"|\0|\b|\f`
}
"#;
        let compilation = compile_source(source).expect("nested template compiles");
        let main_id = compilation
            .module
            .functions
            .iter()
            .position(|function| function.name.ends_with("::main"))
            .expect("main bytecode");
        let main = &compilation.module.functions[main_id];
        let calls = main
            .code
            .iter()
            .filter_map(|instruction| match instruction {
                Instruction::DirectCall { function, .. } => Some(
                    compilation.module.functions[*function as usize]
                        .name
                        .as_str(),
                ),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].ends_with("::first"), "{calls:?}");
        assert!(calls[1].ends_with("::second"), "{calls:?}");
        let concatenations = main
            .code
            .iter()
            .enumerate()
            .filter(|(_, instruction)| {
                matches!(
                    instruction,
                    Instruction::StringCall {
                        operation: StringOperation::TemplateConcat,
                        ..
                    }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(concatenations.len(), 2);
        for (instruction, _) in concatenations {
            let location = compilation
                .debug
                .locations
                .iter()
                .find(|location| {
                    location.function as usize == main_id
                        && location.instruction as usize == instruction
                })
                .expect("template concat debug location");
            let text = &source[location.start as usize..location.end as usize];
            assert!(text.starts_with('`') && text.ends_with('`'), "{text:?}");
        }
        let verified = verify(compilation.module).expect("nested template bytecode verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap(),
            allen_vm::Value::String("// /* {} NAB|`|${|\\|\n|\r|\t|\"|\0|\x08|\x0c".into())
        );
    }

    #[test]
    fn strings_string_iteration_is_snapshot_scalar_get_control_flow() {
        let source = r#"
fn make() returns String { "aé𝄞" }
export fn main() returns (String, Int, Bool, String) {
  mut source = make();
  mut seen = "";
  mut index = 0;
  mut equivalent = true;
  for scalar in source {
    let expected = match string.get("aé𝄞", index) {
      Some(value) => value,
      None => ""
    };
    equivalent = equivalent && scalar == expected;
    index += 1;
    source = "";
    if (index == 1) { continue; }
    seen = string.concat(seen, scalar);
    if (index == 3) { break; }
  }
  mut empty = "ok";
  for scalar in "" { empty = scalar; }
  (seen, index, equivalent, empty)
}
"#;
        let compilation = compile_source(source).expect("string iteration compiles");
        for function in &compilation.mir.functions {
            function.validate_cfg().expect("string loop MIR validates");
        }
        let main_id = compilation
            .module
            .functions
            .iter()
            .position(|function| function.name.ends_with("::main"))
            .expect("main bytecode");
        let main = &compilation.module.functions[main_id];
        assert_eq!(
            main.code
                .iter()
                .filter(|instruction| matches!(instruction, Instruction::DirectCall { .. }))
                .count(),
            1,
            "the iterable source must be evaluated exactly once"
        );
        assert!(
            main.code
                .iter()
                .any(|instruction| matches!(instruction, Instruction::SwitchEnum { .. })),
            "string.get must be safely unwrapped through Option"
        );
        for instruction in &main.code {
            if let Instruction::IndexGet { collection, .. } = instruction {
                assert_ne!(
                    main.registers[*collection as usize],
                    ValueType::String,
                    "String iteration must not lower through IndexGet"
                );
            }
        }
        let internal_get = main
            .code
            .iter()
            .position(|instruction| {
                matches!(
                    instruction,
                    Instruction::StringCall {
                        operation: StringOperation::Get,
                        ..
                    }
                )
            })
            .expect("internal string.get");
        let location = compilation
            .debug
            .locations
            .iter()
            .find(|location| {
                location.function as usize == main_id
                    && location.instruction as usize == internal_get
            })
            .expect("string iteration get debug location");
        assert_eq!(
            &source[location.start as usize..location.end as usize],
            "scalar"
        );
        let verified = verify(compilation.module).expect("string iteration CFG verifies");
        assert_eq!(
            allen_vm::execute(&verified).unwrap(),
            allen_vm::Value::Tuple(
                vec![
                    allen_vm::Value::String("é𝄞".into()),
                    allen_vm::Value::Int(3),
                    allen_vm::Value::Bool(true),
                    allen_vm::Value::String("ok".into()),
                ]
                .into()
            )
        );
    }

    #[test]
    fn strings_and_templates_reject_obsolete_or_invalid_forms_exactly() {
        for (source, code, text) in [
            (
                "export fn main() returns String { \"text\"[0] }",
                "E2009",
                "\"text\"[0]",
            ),
            (
                "export fn main() returns String { \"a\" + \"b\" }",
                "E2003",
                "\"a\" + \"b\"",
            ),
            (
                "export fn main() returns String { mut value = \"\"; value += \"x\"; value }",
                "E2003",
                "value",
            ),
            ("export fn main() returns String { `value ${1}` }", "E3011", "1"),
        ] {
            let diagnostics = compile_source(source).expect_err("invalid String form is rejected");
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert_eq!(diagnostics[0].code, code, "{source}");
            assert_eq!(
                &source[diagnostics[0].span.start..diagnostics[0].span.end],
                text,
                "{source}"
            );
        }

        let malformed = [
            r#"export fn main() returns String { `bad \q` }"#.to_owned(),
            "export fn main() returns String { `bad\nline` }".to_owned(),
            "export fn main() returns String { `bad\0control` }".to_owned(),
            r#"export fn main() returns String { `unterminated"#.to_owned(),
            r#"export fn main() returns String { `unterminated ${1"#.to_owned(),
        ];
        for source in malformed {
            let diagnostics = compile_source(&source).expect_err("malformed template is rejected");
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E0004", "{source}");
            assert!(diagnostics[0].span.start < diagnostics[0].span.end);
            assert!(diagnostics[0].span.end <= source.len());
        }
    }

    #[test]
    fn strings_capability_inspection_requires_a_direct_explicit_effect() {
        for source in [
            r#"
fn inspect() returns Bool { capability.is_granted("fs.read") }
export fn main() returns Bool effects [capability.inspect] { inspect() }
"#,
            r#"
fn inspect() returns List<String> { capability.granted() }
export fn main() returns List<String> effects [capability.inspect] { inspect() }
"#,
            r#"
export fn main() returns Bool effects [capability.inspect] {
  let inspect = fn() returns Bool { capability.is_granted("fs.read") };
  inspect()
}
"#,
        ] {
            let diagnostics = compile_source(source)
                .expect_err("direct capability inspection needs an explicit effect");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].code, "E2403");
            assert!(diagnostics[0].message.contains("explicitly declare effect"));
        }

        let source = r#"
fn inspect() returns Bool effects [capability.inspect] {
  capability.is_granted("fs.read") || length(capability.granted()) != 0
}
fn relay() returns Bool effects [capability.inspect] { inspect() }
export fn main() returns Bool effects [capability.inspect] { relay() }
"#;
        let compilation =
            compile_source(source).expect("direct and transitive declarations compile");
        let relay = compilation
            .module
            .functions
            .iter()
            .find(|function| function.name.ends_with("::relay"))
            .expect("relay bytecode");
        assert_eq!(
            compilation.module.effect_sets[relay.effects as usize],
            vec!["capability.inspect".to_owned()]
        );
        let operations = compilation
            .module
            .functions
            .iter()
            .flat_map(|function| &function.code)
            .filter_map(|instruction| match instruction {
                Instruction::CapabilityInspect { operation, .. } => Some(*operation),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            [CapabilityOperation::IsGranted, CapabilityOperation::Granted]
        );
        assert!(
            compilation
                .hir
                .modules
                .iter()
                .flat_map(|module| &module.functions)
                .any(|function| {
                    fn contains_capability(expression: &HirExpr) -> bool {
                        match &expression.kind {
                            HirExprKind::CapabilityInspect { .. } => true,
                            HirExprKind::Block(values)
                            | HirExprKind::Tuple(values)
                            | HirExprKind::DirectCall(values)
                            | HirExprKind::AsyncCall(values)
                            | HirExprKind::ClosureCall(values)
                            | HirExprKind::List(values)
                            | HirExprKind::Binary(values) => values.iter().any(contains_capability),
                            _ => false,
                        }
                    }
                    contains_capability(&function.body)
                })
        );
        assert!(compilation.mir.functions.iter().any(|function| {
            function.blocks.iter().any(|block| {
                block
                    .operations
                    .iter()
                    .any(|operation| matches!(operation, MirOperation::CapabilityInspect { .. }))
            })
        }));
        verify(compilation.module).expect("capability inspection bytecode verifies");
    }
