#![no_main]

use allen_bytecode::{
    Artifact, ArtifactMetadata, BYTECODE_VERSION, CheckedIntOperation, DecodeLimits, EntryContract,
    EnumPayloadType, EnumType, EnumVariant, Function, Instruction, ManifestContract, Module,
    SafeCollectionOperation, StrictSchema, ToolContract, ToolVerificationContract, ValueType,
    compute_strict_schema_digest, compute_tool_contract_digest, decode_and_verify, encode,
    standard_error_type, verify_with_frozen_tool_catalog,
};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 1024 * 1024;

fn mutation_kind(data: &[u8]) -> u8 {
    data.get(2).copied().unwrap_or_default() % 6
}

fn current_artifact(data: &[u8]) -> Artifact {
    let selector = data.first().copied().unwrap_or_default();
    let mutation = mutation_kind(data);
    let wrong_wrapper_identity = data.get(3).is_some_and(|byte| byte & 1 == 1);
    let (safe_operation, safe_arguments, safe_result) = match selector % 4 {
        0 => (
            SafeCollectionOperation::ListGet,
            vec![ValueType::List(Box::new(ValueType::Int)), ValueType::Int],
            ValueType::Option(Box::new(ValueType::Int)),
        ),
        1 => (
            SafeCollectionOperation::ListTrySet,
            vec![
                ValueType::List(Box::new(ValueType::Int)),
                ValueType::Int,
                ValueType::Int,
            ],
            ValueType::Option(Box::new(ValueType::List(Box::new(ValueType::Int)))),
        ),
        2 => (
            SafeCollectionOperation::BytesGet,
            vec![ValueType::Bytes, ValueType::Int],
            ValueType::Option(Box::new(ValueType::Int)),
        ),
        _ => (
            SafeCollectionOperation::MapGet,
            vec![
                ValueType::Map(Box::new(ValueType::String), Box::new(ValueType::Int)),
                ValueType::String,
            ],
            ValueType::Option(Box::new(ValueType::Int)),
        ),
    };
    let safe_destination = u16::try_from(safe_arguments.len()).unwrap();
    let mut safe_registers = safe_arguments.clone();
    safe_registers.push(safe_result.clone());
    let mut safe_instruction = Instruction::SafeCollectionCall {
        destination: safe_destination,
        operation: safe_operation,
        arguments: (0..safe_destination).collect(),
    };
    if mutation == 3 {
        let Instruction::SafeCollectionCall { arguments, .. } = &mut safe_instruction else {
            unreachable!()
        };
        arguments.pop();
    }

    let checked_operation = match selector % 6 {
        0 => CheckedIntOperation::Add,
        1 => CheckedIntOperation::Subtract,
        2 => CheckedIntOperation::Multiply,
        3 => CheckedIntOperation::Divide,
        4 => CheckedIntOperation::Remainder,
        _ => CheckedIntOperation::Negate,
    };
    let checked_arity = usize::from(checked_operation != CheckedIntOperation::Negate) + 1;
    let checked_destination = u16::try_from(checked_arity).unwrap();
    let mut checked_registers = vec![ValueType::Int; checked_arity];
    checked_registers.push(ValueType::Option(Box::new(ValueType::Int)));
    if mutation == 4 {
        checked_registers[0] = ValueType::Bool;
    }

    let ValueType::Record(standard_fields) = standard_error_type() else {
        unreachable!("standard error is a record")
    };
    let mut wrapper = EnumType {
        name: if wrong_wrapper_identity {
            "pkg://fuzz@0.1.0/src/main.allen::_tool_tools_x2E_other_x2E_echo_x3A__x3A_Error"
        } else {
            "pkg://fuzz@0.1.0/src/main.allen::_tool_tools_x2E_demo_x2E_echo_x3A__x3A_Error"
        }
        .to_owned(),
        variants: vec![
            EnumVariant {
                name: "Declared".to_owned(),
                payload: EnumPayloadType::Tuple(vec![ValueType::String]),
            },
            EnumVariant {
                name: "Unavailable".to_owned(),
                payload: EnumPayloadType::Record(standard_fields.clone()),
            },
            EnumVariant {
                name: "Schema".to_owned(),
                payload: EnumPayloadType::Record(standard_fields),
            },
        ],
    };
    if mutation == 1 {
        wrapper.variants.swap(1, 2);
    }
    let tool_error = if mutation != 2 {
        ValueType::Enum(0)
    } else {
        standard_error_type()
    };
    let tool_result = ValueType::Result(Box::new(ValueType::String), Box::new(tool_error));

    let schemas = vec![
        StrictSchema {
            value_type: ValueType::Unit,
        },
        StrictSchema {
            value_type: ValueType::String,
        },
        StrictSchema {
            value_type: tool_result.clone(),
        },
    ];
    let tool = ToolContract {
        name: "demo.echo".to_owned(),
        version: "1.0.0".to_owned(),
        version_requirement: ">=1.0.0, <2.0.0".to_owned(),
        effect: "tool.demo.echo@1".to_owned(),
        input_schema: 0,
        output_schema: 1,
        error_schema: 1,
        input_digest: compute_strict_schema_digest(&schemas[0]),
        output_digest: compute_strict_schema_digest(&schemas[1]),
        error_digest: compute_strict_schema_digest(&schemas[1]),
    };
    let tool_index = u32::from(mutation == 5);

    Artifact {
        metadata: ArtifactMetadata {
            bytecode_version: BYTECODE_VERSION,
            ..ArtifactMetadata::default()
        },
        module: Module {
            constants: vec![],
            enum_types: vec![wrapper],
            effect_sets: vec![vec![], vec![tool.effect.clone()]],
            functions: vec![
                Function {
                    name: "pkg/x66757a7a/x302e312e30/x737263/x6d61696e.allen::safe".to_owned(),
                    parameters: (0..safe_destination).collect(),
                    captures: vec![],
                    registers: safe_registers,
                    return_type: safe_result,
                    effects: 0,
                    code: vec![
                        safe_instruction,
                        Instruction::Return {
                            source: safe_destination,
                        },
                    ],
                },
                Function {
                    name: "pkg/x66757a7a/x302e312e30/x737263/x6d61696e.allen::checked".to_owned(),
                    parameters: (0..checked_destination).collect(),
                    captures: vec![],
                    registers: checked_registers,
                    return_type: ValueType::Option(Box::new(ValueType::Int)),
                    effects: 0,
                    code: vec![
                        Instruction::CheckedIntCall {
                            destination: checked_destination,
                            operation: checked_operation,
                            arguments: (0..checked_destination).collect(),
                        },
                        Instruction::Return {
                            source: checked_destination,
                        },
                    ],
                },
                Function {
                    name: "pkg/x66757a7a/x302e312e30/x737263/x6d61696e.allen::main".to_owned(),
                    parameters: vec![0],
                    captures: vec![],
                    registers: vec![
                        ValueType::Unit,
                        ValueType::Future(Box::new(tool_result.clone())),
                        tool_result.clone(),
                    ],
                    return_type: tool_result,
                    effects: 1,
                    code: vec![
                        Instruction::ToolInvoke {
                            destination: 1,
                            tool: tool_index,
                            input: 0,
                        },
                        Instruction::Await {
                            destination: 2,
                            source: 1,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
            ],
            async_functions: vec![2],
            entry: 0,
        },
        debug: None,
        schemas,
        entries: vec![EntryContract {
            name: "main".to_owned(),
            function: 2,
            input_schema: 0,
            output_schema: 2,
        }],
        imports: vec![],
        manifest: Some(ManifestContract {
            package: "fuzz".to_owned(),
            version: "0.1.0".to_owned(),
            language_requirement: "0.1".to_owned(),
            required_capabilities: vec![],
            optional_capabilities: vec![],
            limits: vec![],
            https_origins: vec![],
            required_tools: vec![tool.clone()],
            tool_contract_digest: compute_tool_contract_digest(&[tool]),
        }),
    }
}

fuzz_target!(|data: &[u8]| {
    let input = &data[..data.len().min(MAX_INPUT_BYTES)];
    let limits = DecodeLimits {
        artifact_bytes: MAX_INPUT_BYTES,
        section_bytes: MAX_INPUT_BYTES,
        string_bytes: 64 * 1024,
        table_entries: 4_096,
        functions: 1_024,
        registers_per_function: 4_096,
        instructions_per_function: 16_384,
        operands_per_instruction: 4_096,
        type_depth: 32,
        debug_records: 4_096,
        verifier_state_bytes: MAX_INPUT_BYTES,
        expanded_type_nodes: 4_096,
        decoded_model_bytes: MAX_INPUT_BYTES,
    };
    let _ = decode_and_verify(input, &limits);
    let artifact = current_artifact(input);
    let catalog = [ToolVerificationContract {
        tool_name: "demo.echo".to_owned(),
        input: ValueType::Unit,
        output: ValueType::String,
        declared_error: ValueType::String,
    }];
    let direct = verify_with_frozen_tool_catalog(artifact.module.clone(), &catalog);
    if input.get(3).is_some_and(|byte| byte & 1 == 1) {
        let error = direct.expect_err("wrong nominal tool wrapper must fail verification");
        if mutation_kind(input) == 0 {
            assert!(
                error
                    .message
                    .contains("tool invocation error wrapper is invalid")
            );
        }
        return;
    }
    match mutation_kind(input) {
        0 => {
            direct.expect("canonical current module must verify against its frozen catalog");
            let encoded = encode(&artifact).expect("canonical current artifact must encode");
            decode_and_verify(&encoded, &limits)
                .expect("canonical current artifact must independently verify");
        }
        1 | 2 => {
            let error = direct.expect_err("invalid tool wrapper must fail direct verification");
            assert!(
                error
                    .message
                    .contains("tool invocation error wrapper is invalid")
            );
        }
        3 => {
            let error = direct.expect_err("invalid safe-operation arity must fail verification");
            assert!(
                error
                    .message
                    .contains("safe collection operation signature is invalid")
            );
        }
        4 => {
            let error = direct.expect_err("invalid checked-integer operand must fail verification");
            assert!(error.message.contains("call argument"));
        }
        5 => {
            let error = direct.expect_err("invalid tool index must fail direct verification");
            assert!(
                error
                    .message
                    .contains("tool invocation contract is out of range")
            );
        }
        _ => unreachable!(),
    }
});
