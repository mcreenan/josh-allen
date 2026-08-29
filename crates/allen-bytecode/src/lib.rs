#![forbid(unsafe_code)]

mod artifact;

pub use artifact::{
    ARTIFACT_MAGIC, Artifact, ArtifactError, ArtifactErrorCode, ArtifactMetadata, BYTECODE_VERSION,
    DebugInfo, DebugLocation, DecodeLimits, DecodedArtifact, EntryContract, EntryRecordProvenance,
    ImportContract, ManifestContract, SectionSummary, SemanticVersion, StrictSchema, TargetProfile,
    ToolContract, VerifiedArtifact, canonical_value_type_bytes, compute_entry_contract_digest,
    compute_entry_record_provenance, compute_strict_schema_digest, compute_template_digest,
    compute_tool_contract_digest, decode, decode_and_verify, encode, encode_with_limits,
};

mod model;
mod verifier;

pub(crate) use model::tool_declared_error_type;
pub use model::*;
pub use verifier::*;
pub(crate) use verifier::{contains_stored_sub_agent, is_canonical_effect_id, verify_internal};

#[cfg(test)]
mod tests {
    use super::model::sub_agent_projection_type;
    use super::{
        BoolBinaryOp, CANONICAL_NAN_BITS, CompareOp, Constant, Conversion, EffectOperation,
        EnumPayloadType, EnumSwitchArm, EnumType, EnumVariant, ExternalFsAccess, FsOperation,
        Function, Instruction, MAX_VALUE_NESTING, Module, NumericBinaryOp, RecordField, Register,
        ValueType, effect_result_type, external_directory_request_type, external_file_request_type,
        file_error_type, prompt_type, task_snapshot_type, verify,
    };

    fn function(
        registers: Vec<ValueType>,
        return_type: ValueType,
        code: Vec<Instruction>,
    ) -> Function {
        Function {
            name: "main".to_owned(),
            parameters: vec![],
            parameter_names: vec![],
            parameter_default_digests: vec![],
            captures: vec![],
            registers,
            return_type,
            effects: 0,
            code,
        }
    }

    fn bytecode_module(
        constants: Vec<Constant>,
        enum_types: Vec<EnumType>,
        function: Function,
    ) -> Module {
        Module {
            constants,
            enum_types,
            effect_sets: vec![vec![]],
            functions: vec![function],
            async_functions: vec![],
            entry: 0,
        }
    }

    fn operation_module(
        registers: Vec<ValueType>,
        parameters: Vec<Register>,
        instruction: Instruction,
        return_register: Register,
    ) -> Module {
        let return_type = registers[return_register as usize].clone();
        let mut operation = function(
            registers,
            return_type,
            vec![
                instruction,
                Instruction::Return {
                    source: return_register,
                },
            ],
        );
        operation.parameters = parameters;
        operation.parameter_names = (0..operation.parameters.len())
            .map(|index| format!("_arg{index}"))
            .collect();
        operation.parameter_default_digests = vec![None; operation.parameters.len()];
        bytecode_module(vec![], vec![], operation)
    }

    fn field(name: &str, value_type: ValueType) -> RecordField {
        RecordField {
            name: name.to_owned(),
            value_type,
        }
    }

    fn enum_type(variants: Vec<EnumVariant>) -> EnumType {
        EnumType {
            name: "Reading".to_owned(),
            variants,
        }
    }

    fn enum_chain(last: usize) -> Vec<EnumType> {
        let mut enum_types = vec![EnumType {
            name: "E0".to_owned(),
            variants: vec![EnumVariant {
                name: "End".to_owned(),
                payload: EnumPayloadType::Unit,
            }],
        }];
        for index in 1..=last {
            enum_types.push(EnumType {
                name: format!("E{index}"),
                variants: vec![EnumVariant {
                    name: "Next".to_owned(),
                    payload: EnumPayloadType::Tuple(vec![ValueType::Enum(
                        u32::try_from(index - 1).expect("test enum index fits"),
                    )]),
                }],
            });
        }
        enum_types
    }

    fn shared_enum_dag(last: usize) -> Vec<EnumType> {
        let mut enum_types = vec![EnumType {
            name: "E0".to_owned(),
            variants: vec![EnumVariant {
                name: "Leaf".to_owned(),
                payload: EnumPayloadType::Unit,
            }],
        }];
        for index in 1..=last {
            let previous = ValueType::Enum(u32::try_from(index - 1).expect("test index fits"));
            enum_types.push(EnumType {
                name: format!("E{index}"),
                variants: vec![EnumVariant {
                    name: "Pair".to_owned(),
                    payload: EnumPayloadType::Tuple(vec![previous.clone(), previous]),
                }],
            });
        }
        enum_types
    }

    #[test]
    fn verifies_integer_addition() {
        let module = Module {
            constants: vec![Constant::Int(40), Constant::Int(2)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::Int, ValueType::Int, ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::IntBinary {
                        destination: 2,
                        left: 0,
                        right: 1,
                        operation: NumericBinaryOp::Add,
                    },
                    Instruction::Return { source: 2 },
                ],
            )],
            async_functions: vec![],
            entry: 0,
        };

        verify(module).expect("module must verify");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn typed_responses_preserve_nested_result_output() {
        fn typed_response_module(
            operation: EffectOperation,
            output: ValueType,
            future_output: ValueType,
        ) -> Module {
            let prompt = prompt_type(output);
            let (mut registers, parameters, arguments) = match operation {
                EffectOperation::AgentAsk
                | EffectOperation::ModelRequest
                | EffectOperation::UserAsk => (vec![prompt], vec![0], vec![0]),
                EffectOperation::SubAgentRun => (
                    vec![prompt, sub_agent_projection_type()],
                    vec![0, 1],
                    vec![0, 1],
                ),
                EffectOperation::SubAgentAsk => {
                    (vec![ValueType::SubAgent, prompt], vec![0, 1], vec![0, 1])
                }
                _ => unreachable!("test covers typed response operations"),
            };
            let future = u16::try_from(registers.len()).unwrap();
            let awaited = future + 1;
            registers.push(ValueType::Future(Box::new(future_output.clone())));
            registers.push(future_output.clone());
            let mut function = function(
                registers,
                future_output,
                vec![
                    Instruction::EffectCall {
                        destination: future,
                        operation,
                        arguments,
                    },
                    Instruction::Await {
                        destination: awaited,
                        source: future,
                    },
                    Instruction::Return { source: awaited },
                ],
            );
            function.parameters = parameters;
            function.parameter_names = (0..function.parameters.len())
                .map(|index| format!("_arg{index}"))
                .collect();
            function.parameter_default_digests = vec![None; function.parameters.len()];
            let mut module = bytecode_module(vec![], vec![], function);
            module.effect_sets[0] = vec![operation.required_effect().to_owned()];
            module.async_functions = vec![0];
            module
        }

        let nested = ValueType::Result(Box::new(ValueType::Int), Box::new(ValueType::Bool));
        for (operation, domain_error) in [
            (EffectOperation::AgentAsk, super::agent_error_type()),
            (EffectOperation::ModelRequest, super::model_error_type()),
            (EffectOperation::UserAsk, super::user_error_type()),
            (EffectOperation::SubAgentRun, super::sub_agent_error_type()),
            (EffectOperation::SubAgentAsk, super::sub_agent_error_type()),
        ] {
            let envelope =
                ValueType::Result(Box::new(nested.clone()), Box::new(domain_error.clone()));
            let valid = typed_response_module(operation, nested.clone(), envelope);
            verify(valid.clone()).expect("the exact typed-response envelope verifies");
            assert_eq!(
                super::typed_response_output_type(&valid.functions[0], &valid.functions[0].code[0],),
                Some(&nested),
            );

            let missing_envelope = typed_response_module(operation, nested.clone(), nested.clone());
            assert_eq!(
                verify(missing_envelope.clone()).unwrap_err().message,
                "effect operation result",
            );
            assert_eq!(
                super::typed_response_output_type(
                    &missing_envelope.functions[0],
                    &missing_envelope.functions[0].code[0],
                ),
                None,
            );

            let wrong_domain_error = typed_response_module(
                operation,
                nested.clone(),
                ValueType::Result(Box::new(nested.clone()), Box::new(ValueType::String)),
            );
            assert_eq!(
                verify(wrong_domain_error.clone()).unwrap_err().message,
                "effect operation result",
            );
            assert_eq!(
                super::typed_response_output_type(
                    &wrong_domain_error.functions[0],
                    &wrong_domain_error.functions[0].code[0],
                ),
                None,
            );

            for mutated_nested in [
                ValueType::Result(Box::new(ValueType::String), Box::new(ValueType::Bool)),
                ValueType::Result(Box::new(ValueType::Int), Box::new(ValueType::String)),
            ] {
                let mutated_envelope =
                    ValueType::Result(Box::new(mutated_nested), Box::new(domain_error.clone()));
                assert_eq!(
                    verify(typed_response_module(
                        operation,
                        nested.clone(),
                        mutated_envelope,
                    ))
                    .unwrap_err()
                    .message,
                    "effect operation result",
                );
            }
        }
    }

    #[test]
    fn rejects_an_uninitialized_return_register() {
        let module = Module {
            constants: vec![],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::Int],
                ValueType::Int,
                vec![Instruction::Return { source: 0 }],
            )],
            async_functions: vec![],
            entry: 0,
        };

        let error = verify(module).expect_err("module must be invalid");

        assert_eq!(error.instruction, Some(0));
        assert_eq!(error.message, "register is not initialized");
    }

    #[test]
    fn rejects_a_return_with_the_wrong_type() {
        let module = Module {
            constants: vec![Constant::Bool(true)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::Bool],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Return { source: 0 },
                ],
            )],
            async_functions: vec![],
            entry: 0,
        };

        assert_eq!(
            verify(module).expect_err("module must be invalid").message,
            "return"
        );
    }

    #[test]
    fn rejects_out_of_range_constant_and_register_references() {
        let bad_constant = Module {
            constants: vec![],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::Int],
                ValueType::Int,
                vec![Instruction::Const {
                    destination: 0,
                    constant: 0,
                }],
            )],
            async_functions: vec![],
            entry: 0,
        };
        assert_eq!(
            verify(bad_constant)
                .expect_err("module must be invalid")
                .message,
            "constant is out of range"
        );

        let bad_register = Module {
            constants: vec![Constant::Int(1)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::Int],
                ValueType::Int,
                vec![Instruction::Const {
                    destination: 1,
                    constant: 0,
                }],
            )],
            async_functions: vec![],
            entry: 0,
        };
        assert_eq!(
            verify(bad_register)
                .expect_err("module must be invalid")
                .message,
            "register is out of range"
        );
    }

    #[test]
    fn rejects_malformed_collection_construction() {
        let module = Module {
            constants: vec![Constant::Bool(true)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::Bool, ValueType::List(Box::new(ValueType::Int))],
                ValueType::List(Box::new(ValueType::Int)),
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::ListNew {
                        destination: 1,
                        elements: vec![0],
                    },
                    Instruction::Return { source: 1 },
                ],
            )],
            async_functions: vec![],
            entry: 0,
        };

        assert_eq!(
            verify(module).expect_err("module must be invalid").message,
            "list element"
        );
    }

    #[test]
    fn verifies_dynamic_collection_operations() {
        let list = ValueType::List(Box::new(ValueType::Int));
        let mut list_function = function(
            vec![
                list.clone(),
                ValueType::Int,
                ValueType::Int,
                ValueType::Int,
                list.clone(),
                list.clone(),
            ],
            list.clone(),
            vec![
                Instruction::Length {
                    destination: 3,
                    collection: 0,
                },
                Instruction::ListAppend {
                    destination: 4,
                    values: 0,
                    value: 1,
                },
                Instruction::ListSet {
                    destination: 5,
                    values: 4,
                    index: 2,
                    value: 1,
                },
                Instruction::Return { source: 5 },
            ],
        );
        list_function.parameters = vec![0, 1, 2];
        list_function.parameter_names = vec!["_arg0".into(), "_arg1".into(), "_arg2".into()];
        list_function.parameter_default_digests = vec![None, None, None];
        let mut bytes_function = function(
            vec![ValueType::Bytes, ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Length {
                    destination: 1,
                    collection: 0,
                },
                Instruction::Return { source: 1 },
            ],
        );
        bytes_function.parameters = vec![0];
        bytes_function.parameter_names = vec!["_arg0".into()];
        bytes_function.parameter_default_digests = vec![None];
        bytes_function.name = "bytes_length".to_owned();
        let module = Module {
            constants: vec![],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![list_function, bytes_function],
            async_functions: vec![],
            entry: 0,
        };

        verify(module).expect("dynamic collection operations must verify");
    }

    #[test]
    fn rejects_dynamic_collection_operations_with_wrong_types() {
        let int_list = ValueType::List(Box::new(ValueType::Int));
        let bool_list = ValueType::List(Box::new(ValueType::Bool));
        let cases = [
            (
                vec![int_list.clone(), int_list.clone()],
                Instruction::Length {
                    destination: 1,
                    collection: 0,
                },
                "length destination",
            ),
            (
                vec![ValueType::Int, ValueType::Int],
                Instruction::Length {
                    destination: 1,
                    collection: 0,
                },
                "length collection must be Bytes, String, List, or Map",
            ),
            (
                vec![int_list.clone(), ValueType::Int],
                Instruction::ListAppend {
                    destination: 1,
                    values: 0,
                    value: 1,
                },
                "list append destination must be List",
            ),
            (
                vec![int_list.clone(), bool_list.clone(), ValueType::Int],
                Instruction::ListAppend {
                    destination: 0,
                    values: 1,
                    value: 2,
                },
                "list append values",
            ),
            (
                vec![int_list.clone(), ValueType::Bool],
                Instruction::ListAppend {
                    destination: 0,
                    values: 0,
                    value: 1,
                },
                "list append value",
            ),
            (
                vec![int_list.clone(), ValueType::Int, ValueType::Bool],
                Instruction::ListSet {
                    destination: 0,
                    values: 0,
                    index: 2,
                    value: 1,
                },
                "list set index",
            ),
            (
                vec![int_list.clone(), ValueType::Bool, ValueType::Int],
                Instruction::ListSet {
                    destination: 0,
                    values: 0,
                    index: 2,
                    value: 1,
                },
                "list set value",
            ),
            (
                vec![int_list.clone(), ValueType::Int, ValueType::Int],
                Instruction::ListSet {
                    destination: 1,
                    values: 0,
                    index: 1,
                    value: 2,
                },
                "list set destination must be List",
            ),
            (
                vec![int_list.clone(), bool_list, ValueType::Int],
                Instruction::ListSet {
                    destination: 0,
                    values: 1,
                    index: 2,
                    value: 2,
                },
                "list set values",
            ),
        ];

        for (registers, instruction, expected) in cases {
            let parameters = (0..registers.len())
                .map(|register| u16::try_from(register).unwrap())
                .collect();
            assert_eq!(
                verify(operation_module(registers, parameters, instruction, 0))
                    .unwrap_err()
                    .message,
                expected
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn accepts_terminal_affine_transfers_without_back_edges() {
        let future_int = ValueType::Future(Box::new(ValueType::Int));
        let task_int = ValueType::Task(Box::new(ValueType::Int));
        let module = Module {
            constants: vec![Constant::Int(1)],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![
                        future_int.clone(),
                        task_int.clone(),
                        task_int.clone(),
                        ValueType::Int,
                        future_int.clone(),
                        future_int.clone(),
                        ValueType::Int,
                        ValueType::Int,
                    ],
                    return_type: ValueType::Int,
                    effects: 1,
                    code: vec![
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 0,
                        },
                        Instruction::DirectCall {
                            destination: 2,
                            function: 3,
                            arguments: vec![1],
                        },
                        Instruction::Await {
                            destination: 3,
                            source: 2,
                        },
                        Instruction::AsyncCall {
                            destination: 4,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::DirectCall {
                            destination: 5,
                            function: 2,
                            arguments: vec![4],
                        },
                        Instruction::Await {
                            destination: 6,
                            source: 5,
                        },
                        Instruction::IntBinary {
                            destination: 7,
                            left: 3,
                            right: 6,
                            operation: NumericBinaryOp::Add,
                        },
                        Instruction::Return { source: 7 },
                    ],
                },
                Function {
                    name: "number".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
                Function {
                    name: "transfer_future".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: vec![],
                    registers: vec![future_int.clone()],
                    return_type: future_int,
                    effects: 0,
                    code: vec![Instruction::Return { source: 0 }],
                },
                Function {
                    name: "transfer_task".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: vec![],
                    registers: vec![task_int.clone()],
                    return_type: task_int,
                    effects: 0,
                    code: vec![Instruction::Return { source: 0 }],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        };

        verify(module.clone()).expect("straight-line affine transfers are accepted");
        verify(module).expect("affine transfers without a back edge are accepted");
    }

    #[test]
    fn normalizes_consumed_iteration_affines_but_rejects_live_values() {
        let main = Function {
            name: "main".to_owned(),
            parameters: vec![],
            parameter_names: vec![],
            parameter_default_digests: vec![],
            captures: vec![],
            registers: vec![
                ValueType::Bool,
                ValueType::Future(Box::new(ValueType::Unit)),
                ValueType::Unit,
            ],
            return_type: ValueType::Unit,
            effects: 0,
            code: vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::BranchBool {
                    condition: 0,
                    true_target: 2,
                    false_target: 5,
                },
                Instruction::AsyncCall {
                    destination: 1,
                    function: 1,
                    arguments: vec![],
                },
                Instruction::Await {
                    destination: 2,
                    source: 1,
                },
                Instruction::Jump { target: 0 },
                Instruction::Const {
                    destination: 2,
                    constant: 1,
                },
                Instruction::Return { source: 2 },
            ],
        };
        let worker = Function {
            name: "worker".to_owned(),
            parameters: vec![],
            parameter_names: vec![],
            parameter_default_digests: vec![],
            captures: vec![],
            registers: vec![ValueType::Unit],
            return_type: ValueType::Unit,
            effects: 0,
            code: vec![
                Instruction::Const {
                    destination: 0,
                    constant: 1,
                },
                Instruction::Return { source: 0 },
            ],
        };
        let consumed = Module {
            constants: vec![Constant::Bool(false), Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![main, worker],
            async_functions: vec![0, 1],
            entry: 0,
        };
        verify(consumed.clone()).expect("consumed per-iteration Future may reach the back edge");

        let mut live = consumed;
        live.functions[0].code = vec![
            Instruction::Const {
                destination: 0,
                constant: 0,
            },
            Instruction::BranchBool {
                condition: 0,
                true_target: 2,
                false_target: 4,
            },
            Instruction::AsyncCall {
                destination: 1,
                function: 1,
                arguments: vec![],
            },
            Instruction::Jump { target: 0 },
            Instruction::Const {
                destination: 2,
                constant: 1,
            },
            Instruction::Return { source: 2 },
        ];
        assert_eq!(
            verify(live).unwrap_err().message,
            "backward control-flow edge cannot carry a live affine value"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn preserves_outer_sub_agent_provenance_through_aliases() {
        let direct = Module {
            constants: vec![Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: vec![],
                    registers: vec![
                        ValueType::SubAgent,
                        ValueType::SubAgent,
                        ValueType::SubAgent,
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Unit,
                    effects: 0,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::Move {
                            destination: 1,
                            source: 0,
                        },
                        Instruction::DirectCall {
                            destination: 2,
                            function: 1,
                            arguments: vec![1],
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Move {
                            destination: 1,
                            source: 2,
                        },
                        Instruction::Const {
                            destination: 3,
                            constant: 0,
                        },
                        Instruction::Return { source: 3 },
                    ],
                },
                Function {
                    name: "identity_handle".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: vec![],
                    registers: vec![ValueType::SubAgent],
                    return_type: ValueType::SubAgent,
                    effects: 0,
                    code: vec![Instruction::Return { source: 0 }],
                },
            ],
            async_functions: vec![0],
            entry: 0,
        };
        verify(direct.clone()).expect("direct SubAgent alias behavior is accepted");
        verify(direct)
            .expect("outer SubAgent Move and DirectCall aliases survive inner scope exit");

        let callback = ValueType::Function {
            parameters: vec![ValueType::SubAgent],
            return_type: Box::new(ValueType::Unit),
            effects: 0,
        };
        let structural = Module {
            constants: vec![Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: vec![],
                    registers: vec![
                        ValueType::SubAgent,
                        callback.clone(),
                        callback,
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Unit,
                    effects: 0,
                    code: vec![
                        Instruction::ClosureNew {
                            destination: 1,
                            function: 1,
                            captures: vec![],
                        },
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::Move {
                            destination: 2,
                            source: 1,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::ClosureCall {
                            destination: 3,
                            closure: 2,
                            arguments: vec![0],
                        },
                        Instruction::Return { source: 3 },
                    ],
                },
                Function {
                    name: "accept_handle".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: vec![],
                    registers: vec![ValueType::SubAgent, ValueType::Unit],
                    return_type: ValueType::Unit,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 1,
                            constant: 0,
                        },
                        Instruction::Return { source: 1 },
                    ],
                },
            ],
            async_functions: vec![0],
            entry: 0,
        };
        verify(structural)
            .expect("structurally SubAgent-containing Move preserves outer provenance");
    }

    #[test]
    fn loop_exit_cannot_read_a_body_only_initialization() {
        let mut loop_function = function(
            vec![ValueType::Bool, ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::BranchBool {
                    condition: 0,
                    true_target: 1,
                    false_target: 3,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 0,
                },
                Instruction::Jump { target: 0 },
                Instruction::Return { source: 1 },
            ],
        );
        loop_function.parameters = vec![0];
        loop_function.parameter_names = vec!["_arg0".into()];
        loop_function.parameter_default_digests = vec![None];
        let module = bytecode_module(vec![Constant::Int(1)], vec![], loop_function);
        assert_eq!(
            verify(module).unwrap_err().message,
            "register is not initialized"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn rejects_invalid_and_uninitialized_dynamic_collection_registers() {
        let list = ValueType::List(Box::new(ValueType::Int));
        for instruction in [
            Instruction::Length {
                destination: 9,
                collection: 0,
            },
            Instruction::Length {
                destination: 1,
                collection: 9,
            },
            Instruction::ListAppend {
                destination: 9,
                values: 0,
                value: 1,
            },
            Instruction::ListAppend {
                destination: 2,
                values: 9,
                value: 1,
            },
            Instruction::ListAppend {
                destination: 2,
                values: 0,
                value: 9,
            },
            Instruction::ListSet {
                destination: 9,
                values: 0,
                index: 1,
                value: 1,
            },
            Instruction::ListSet {
                destination: 2,
                values: 9,
                index: 1,
                value: 1,
            },
            Instruction::ListSet {
                destination: 2,
                values: 0,
                index: 9,
                value: 1,
            },
            Instruction::ListSet {
                destination: 2,
                values: 0,
                index: 1,
                value: 9,
            },
        ] {
            assert_eq!(
                verify(operation_module(
                    vec![list.clone(), ValueType::Int, list.clone()],
                    vec![0, 1],
                    instruction,
                    0,
                ))
                .unwrap_err()
                .message,
                "register is out of range"
            );
        }

        assert_eq!(
            verify(operation_module(
                vec![list.clone(), ValueType::Int],
                vec![],
                Instruction::Length {
                    destination: 1,
                    collection: 0,
                },
                1,
            ))
            .unwrap_err()
            .message,
            "register is not initialized"
        );

        for (instruction, parameters) in [
            (
                Instruction::ListAppend {
                    destination: 3,
                    values: 0,
                    value: 1,
                },
                vec![1],
            ),
            (
                Instruction::ListAppend {
                    destination: 3,
                    values: 0,
                    value: 1,
                },
                vec![0],
            ),
            (
                Instruction::ListSet {
                    destination: 3,
                    values: 0,
                    index: 1,
                    value: 2,
                },
                vec![1, 2],
            ),
            (
                Instruction::ListSet {
                    destination: 3,
                    values: 0,
                    index: 1,
                    value: 2,
                },
                vec![0, 2],
            ),
            (
                Instruction::ListSet {
                    destination: 3,
                    values: 0,
                    index: 1,
                    value: 2,
                },
                vec![0, 1],
            ),
        ] {
            assert_eq!(
                verify(operation_module(
                    vec![list.clone(), ValueType::Int, ValueType::Int, list.clone(),],
                    parameters,
                    instruction,
                    3,
                ))
                .unwrap_err()
                .message,
                "register is not initialized"
            );
        }
    }

    #[test]
    fn rejects_malformed_collection_types() {
        for invalid_type in [
            ValueType::Tuple(vec![]),
            ValueType::Map(Box::new(ValueType::Float), Box::new(ValueType::Int)),
        ] {
            let module = Module {
                constants: vec![],
                enum_types: vec![],
                effect_sets: vec![vec![]],
                functions: vec![function(vec![invalid_type.clone()], invalid_type, vec![])],
                async_functions: vec![],
                entry: 0,
            };

            verify(module).expect_err("module must be invalid");
        }
    }

    #[test]
    fn rejects_sub_agent_hidden_in_unknown_or_enum_payloads() {
        let mut direct = function(
            vec![ValueType::SubAgent, ValueType::Unknown],
            ValueType::Unknown,
            vec![
                Instruction::ToUnknown {
                    destination: 1,
                    source: 0,
                },
                Instruction::Return { source: 1 },
            ],
        );
        direct.parameters = vec![0];
        direct.parameter_names = vec!["_arg0".into()];
        direct.parameter_default_digests = vec![None];
        assert_eq!(
            verify(bytecode_module(vec![], vec![], direct))
                .expect_err("SubAgent must remain opaque")
                .message,
            "to_unknown source cannot be Function, Future, Task, or Never"
        );

        let hidden = bytecode_module(
            vec![],
            vec![EnumType {
                name: "HiddenSubAgent".to_owned(),
                variants: vec![EnumVariant {
                    name: "Value".to_owned(),
                    payload: EnumPayloadType::Tuple(vec![ValueType::SubAgent]),
                }],
            }],
            function(vec![ValueType::Unit], ValueType::Unit, vec![]),
        );
        assert_eq!(
            verify(hidden)
                .expect_err("SubAgent cannot be stored in an enum")
                .message,
            "SubAgent cannot be stored in enum payloads"
        );
    }

    #[test]
    fn rejects_excessive_value_type_nesting() {
        let mut value_type = ValueType::Int;
        for _ in 0..=super::MAX_VALUE_NESTING {
            value_type = ValueType::List(Box::new(value_type));
        }
        let module = Module {
            constants: vec![],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(vec![], value_type, vec![])],
            async_functions: vec![],
            entry: 0,
        };

        assert_eq!(
            verify(module).expect_err("module must be invalid").message,
            "value type nesting exceeds limit"
        );
    }

    #[test]
    fn rejects_an_invalid_conversion_pair() {
        let module = Module {
            constants: vec![Constant::Bool(true)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::Bool, ValueType::Bytes],
                ValueType::Bytes,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Convert {
                        destination: 1,
                        source: 0,
                        conversion: Conversion::StringToBytes,
                    },
                    Instruction::Return { source: 1 },
                ],
            )],
            async_functions: vec![],
            entry: 0,
        };

        assert_eq!(
            verify(module).expect_err("module must be invalid").message,
            "StringToBytes source"
        );
    }

    #[test]
    fn rejects_an_invalid_index_form() {
        let module = Module {
            constants: vec![Constant::String("x".to_owned()), Constant::Bool(true)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::String, ValueType::Bool, ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::IndexGet {
                        destination: 2,
                        collection: 0,
                        index: 1,
                    },
                    Instruction::Return { source: 2 },
                ],
            )],
            async_functions: vec![],
            entry: 0,
        };

        assert_eq!(
            verify(module).expect_err("module must be invalid").message,
            "index collection must be List, Map, or Bytes"
        );
    }

    #[test]
    fn rejects_a_noncanonical_nan_constant() {
        let module = Module {
            constants: vec![Constant::Float(CANONICAL_NAN_BITS | 1)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::Float],
                ValueType::Float,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Return { source: 0 },
                ],
            )],
            async_functions: vec![],
            entry: 0,
        };

        assert_eq!(
            verify(module).expect_err("module must be invalid").message,
            "Float constant has noncanonical NaN bits"
        );
    }

    #[test]
    fn rejects_direct_never_initialization_and_return() {
        let initialization = Module {
            constants: vec![Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::Never],
                ValueType::Never,
                vec![Instruction::Const {
                    destination: 0,
                    constant: 0,
                }],
            )],
            async_functions: vec![],
            entry: 0,
        };
        assert_eq!(
            verify(initialization)
                .expect_err("module must be invalid")
                .message,
            "cannot initialize Never register"
        );

        let return_module = Module {
            constants: vec![Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![ValueType::Unit],
                ValueType::Never,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Return { source: 0 },
                ],
            )],
            async_functions: vec![],
            entry: 0,
        };
        assert_eq!(
            verify(return_module)
                .expect_err("module must be invalid")
                .message,
            "ordinary return cannot return Never"
        );
    }

    #[test]
    fn verifies_comparison_and_boolean_operations() {
        let module = Module {
            constants: vec![Constant::Int(1), Constant::Int(2)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function(
                vec![
                    ValueType::Int,
                    ValueType::Int,
                    ValueType::Bool,
                    ValueType::Bool,
                ],
                ValueType::Bool,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::Compare {
                        destination: 2,
                        left: 0,
                        right: 1,
                        operation: CompareOp::Less,
                    },
                    Instruction::BoolBinary {
                        destination: 3,
                        left: 2,
                        right: 2,
                        operation: BoolBinaryOp::And,
                    },
                    Instruction::Return { source: 3 },
                ],
            )],
            async_functions: vec![],
            entry: 0,
        };

        verify(module).expect("module must verify");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn rejects_malformed_contract_metadata() {
        let unsorted_record = bytecode_module(
            vec![],
            vec![],
            function(
                vec![],
                ValueType::Record(vec![
                    field("z", ValueType::Int),
                    field("a", ValueType::Bool),
                ]),
                vec![],
            ),
        );
        assert_eq!(
            verify(unsorted_record)
                .expect_err("module must be invalid")
                .message,
            "record fields must be unique and sorted by UTF-8 bytes"
        );

        let duplicate_variants = bytecode_module(
            vec![],
            vec![enum_type(vec![
                EnumVariant {
                    name: "Same".to_owned(),
                    payload: EnumPayloadType::Unit,
                },
                EnumVariant {
                    name: "Same".to_owned(),
                    payload: EnumPayloadType::Unit,
                },
            ])],
            function(vec![], ValueType::Unit, vec![]),
        );
        assert_eq!(
            verify(duplicate_variants)
                .expect_err("module must be invalid")
                .message,
            "enum variant names must be nonempty and unique"
        );

        let bad_type_id =
            bytecode_module(vec![], vec![], function(vec![], ValueType::Enum(0), vec![]));
        assert_eq!(
            verify(bad_type_id)
                .expect_err("module must be invalid")
                .message,
            "enum type ID is out of range"
        );

        let recursive_enum = bytecode_module(
            vec![],
            vec![enum_type(vec![EnumVariant {
                name: "Next".to_owned(),
                payload: EnumPayloadType::Tuple(vec![ValueType::Enum(0)]),
            }])],
            function(vec![], ValueType::Unit, vec![]),
        );
        assert_eq!(
            verify(recursive_enum)
                .expect_err("module must be invalid")
                .message,
            "recursive enum types are not supported"
        );

        let exact_depth = bytecode_module(
            vec![Constant::Unit],
            enum_chain(MAX_VALUE_NESTING),
            function(
                vec![ValueType::Unit],
                ValueType::Unit,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Return { source: 0 },
                ],
            ),
        );
        verify(exact_depth).expect("enum nesting at the limit must verify");

        let wrapped_exact_depth = bytecode_module(
            vec![Constant::Unit],
            enum_chain(MAX_VALUE_NESTING - 1),
            function(
                vec![
                    ValueType::Option(Box::new(ValueType::Enum(
                        u32::try_from(MAX_VALUE_NESTING - 1).expect("test enum index fits"),
                    ))),
                    ValueType::Unit,
                ],
                ValueType::Unit,
                vec![
                    Instruction::Const {
                        destination: 1,
                        constant: 0,
                    },
                    Instruction::Return { source: 1 },
                ],
            ),
        );
        verify(wrapped_exact_depth).expect("wrapped enum nesting at the limit must verify");

        let wrapped_excessive_depth = bytecode_module(
            vec![Constant::Unit],
            enum_chain(MAX_VALUE_NESTING),
            function(
                vec![
                    ValueType::Option(Box::new(ValueType::Enum(
                        u32::try_from(MAX_VALUE_NESTING).expect("test enum index fits"),
                    ))),
                    ValueType::Unit,
                ],
                ValueType::Unit,
                vec![
                    Instruction::Const {
                        destination: 1,
                        constant: 0,
                    },
                    Instruction::Return { source: 1 },
                ],
            ),
        );
        assert_eq!(
            verify(wrapped_excessive_depth)
                .expect_err("module must be invalid")
                .message,
            "value type nesting exceeds limit"
        );

        let excessive_depth = bytecode_module(
            vec![],
            enum_chain(MAX_VALUE_NESTING + 1),
            function(vec![], ValueType::Unit, vec![]),
        );
        assert_eq!(
            verify(excessive_depth)
                .expect_err("module must be invalid")
                .message,
            "enum type nesting exceeds limit"
        );
    }

    #[test]
    fn rejects_malformed_record_and_enum_operands() {
        let record_type = ValueType::Record(vec![field("value", ValueType::Int)]);
        let bad_field = bytecode_module(
            vec![Constant::Int(1)],
            vec![],
            function(
                vec![ValueType::Int, record_type.clone(), ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::RecordNew {
                        destination: 1,
                        fields: vec![(0, 0)],
                    },
                    Instruction::FieldGet {
                        destination: 2,
                        record: 1,
                        field: 1,
                    },
                    Instruction::Return { source: 2 },
                ],
            ),
        );
        assert_eq!(
            verify(bad_field)
                .expect_err("module must be invalid")
                .message,
            "record field is out of range"
        );

        let metadata = vec![enum_type(vec![EnumVariant {
            name: "Number".to_owned(),
            payload: EnumPayloadType::Tuple(vec![ValueType::Int]),
        }])];
        let wrong_payload = bytecode_module(
            vec![Constant::Bool(true)],
            metadata.clone(),
            function(
                vec![ValueType::Bool, ValueType::Enum(0)],
                ValueType::Enum(0),
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::EnumNew {
                        destination: 1,
                        variant: 0,
                        payload: vec![0],
                    },
                    Instruction::Return { source: 1 },
                ],
            ),
        );
        assert_eq!(
            verify(wrong_payload)
                .expect_err("module must be invalid")
                .message,
            "enum payload"
        );

        let bad_variant = bytecode_module(
            vec![],
            metadata,
            function(
                vec![ValueType::Enum(0)],
                ValueType::Enum(0),
                vec![
                    Instruction::EnumNew {
                        destination: 0,
                        variant: 1,
                        payload: vec![],
                    },
                    Instruction::Return { source: 0 },
                ],
            ),
        );
        assert_eq!(
            verify(bad_variant)
                .expect_err("module must be invalid")
                .message,
            "enum variant is out of range"
        );
    }

    #[test]
    fn rejects_invalid_control_flow_targets_and_joins() {
        let invalid_target = bytecode_module(
            vec![],
            vec![],
            function(
                vec![],
                ValueType::Unit,
                vec![Instruction::Jump { target: 1 }],
            ),
        );
        assert_eq!(
            verify(invalid_target)
                .expect_err("module must be invalid")
                .message,
            "control-flow target is out of range"
        );

        let bad_join = bytecode_module(
            vec![Constant::Bool(true), Constant::Int(1)],
            vec![],
            function(
                vec![ValueType::Bool, ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::BranchBool {
                        condition: 0,
                        true_target: 2,
                        false_target: 4,
                    },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::Jump { target: 5 },
                    Instruction::Jump { target: 5 },
                    Instruction::Return { source: 1 },
                ],
            ),
        );
        let error = verify(bad_join).expect_err("module must be invalid");
        assert_eq!(error.instruction, Some(5));
        assert_eq!(error.message, "register is not initialized");

        let unreachable = bytecode_module(
            vec![Constant::Int(1)],
            vec![],
            function(
                vec![ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Jump { target: 2 },
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Return { source: 0 },
                ],
            ),
        );
        assert_eq!(
            verify(unreachable)
                .expect_err("module must be invalid")
                .message,
            "instruction is unreachable"
        );
    }

    #[test]
    fn branch_conditions_are_exactly_boolean() {
        let module = bytecode_module(
            vec![Constant::Int(1)],
            vec![],
            function(
                vec![ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::BranchBool {
                        condition: 0,
                        true_target: 2,
                        false_target: 2,
                    },
                    Instruction::Return { source: 0 },
                ],
            ),
        );

        assert_eq!(
            verify(module)
                .expect_err("integer conditions must be rejected")
                .message,
            "Boolean branch condition"
        );
    }

    #[test]
    fn verifies_switch_bindings_only_on_their_edge() {
        let option_int = ValueType::Option(Box::new(ValueType::Int));
        let valid = bytecode_module(
            vec![Constant::Int(7)],
            vec![],
            function(
                vec![option_int.clone(), ValueType::Int, ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 1,
                        constant: 0,
                    },
                    Instruction::EnumNew {
                        destination: 0,
                        variant: 1,
                        payload: vec![1],
                    },
                    Instruction::SwitchEnum {
                        source: 0,
                        arms: vec![
                            EnumSwitchArm {
                                variant: 0,
                                target: 3,
                                bindings: vec![],
                            },
                            EnumSwitchArm {
                                variant: 1,
                                target: 4,
                                bindings: vec![2],
                            },
                        ],
                    },
                    Instruction::Return { source: 1 },
                    Instruction::Return { source: 2 },
                ],
            ),
        );
        verify(valid).expect("module must verify");

        let incomplete = bytecode_module(
            vec![Constant::Int(7)],
            vec![],
            function(
                vec![option_int, ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 1,
                        constant: 0,
                    },
                    Instruction::EnumNew {
                        destination: 0,
                        variant: 1,
                        payload: vec![1],
                    },
                    Instruction::SwitchEnum {
                        source: 0,
                        arms: vec![EnumSwitchArm {
                            variant: 1,
                            target: 3,
                            bindings: vec![1],
                        }],
                    },
                    Instruction::Return { source: 1 },
                ],
            ),
        );
        assert_eq!(
            verify(incomplete)
                .expect_err("module must be invalid")
                .message,
            "enum switch is not exhaustive"
        );
    }

    #[test]
    fn rejects_incompatible_try_and_unknown_operations() {
        let result_int_string =
            ValueType::Result(Box::new(ValueType::Int), Box::new(ValueType::String));
        let incompatible_try = bytecode_module(
            vec![Constant::String("error".to_owned())],
            vec![],
            function(
                vec![ValueType::String, result_int_string, ValueType::Int],
                ValueType::Result(Box::new(ValueType::Bool), Box::new(ValueType::Bytes)),
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::EnumNew {
                        destination: 1,
                        variant: 1,
                        payload: vec![0],
                    },
                    Instruction::TryResult {
                        destination: 2,
                        source: 1,
                    },
                    Instruction::EnumNew {
                        destination: 1,
                        variant: 0,
                        payload: vec![2],
                    },
                    Instruction::Return { source: 1 },
                ],
            ),
        );
        assert_eq!(
            verify(incompatible_try)
                .expect_err("module must be invalid")
                .message,
            "try error type"
        );

        let bad_widen = bytecode_module(
            vec![Constant::Int(1)],
            vec![],
            function(
                vec![ValueType::Int, ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::ToUnknown {
                        destination: 1,
                        source: 0,
                    },
                    Instruction::Return { source: 1 },
                ],
            ),
        );
        assert_eq!(
            verify(bad_widen)
                .expect_err("module must be invalid")
                .message,
            "to_unknown destination"
        );

        let bad_narrow = bytecode_module(
            vec![Constant::Int(1)],
            vec![],
            function(
                vec![ValueType::Int, ValueType::Option(Box::new(ValueType::Int))],
                ValueType::Option(Box::new(ValueType::Int)),
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Narrow {
                        destination: 1,
                        source: 0,
                        target: ValueType::Int,
                    },
                    Instruction::Return { source: 1 },
                ],
            ),
        );
        assert_eq!(
            verify(bad_narrow)
                .expect_err("module must be invalid")
                .message,
            "narrow source"
        );
    }

    #[test]
    fn rejects_unknown_equality_hidden_in_enum_payload() {
        let metadata = vec![enum_type(vec![EnumVariant {
            name: "Value".to_owned(),
            payload: EnumPayloadType::Tuple(vec![ValueType::Unknown]),
        }])];
        let enum_value = ValueType::Enum(0);
        let module = bytecode_module(
            vec![Constant::Int(1)],
            metadata,
            function(
                vec![
                    ValueType::Int,
                    ValueType::Unknown,
                    enum_value.clone(),
                    ValueType::Bool,
                ],
                ValueType::Bool,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::ToUnknown {
                        destination: 1,
                        source: 0,
                    },
                    Instruction::EnumNew {
                        destination: 2,
                        variant: 0,
                        payload: vec![1],
                    },
                    Instruction::Compare {
                        destination: 3,
                        left: 2,
                        right: 2,
                        operation: CompareOp::Equal,
                    },
                    Instruction::Return { source: 3 },
                ],
            ),
        );
        assert_eq!(
            verify(module).expect_err("module must be invalid").message,
            "comparison is not supported for operand type"
        );
    }

    #[test]
    fn enum_properties_are_memoized_for_shared_metadata_dags() {
        const DEPTH: usize = 100;
        let enum_value = ValueType::Enum(u32::try_from(DEPTH).expect("test index fits"));
        let module = bytecode_module(
            vec![],
            shared_enum_dag(DEPTH),
            function(
                vec![
                    enum_value.clone(),
                    enum_value.clone(),
                    ValueType::Bool,
                    ValueType::Unknown,
                    ValueType::Option(Box::new(enum_value.clone())),
                ],
                ValueType::Bool,
                vec![
                    Instruction::Compare {
                        destination: 2,
                        left: 0,
                        right: 1,
                        operation: CompareOp::Equal,
                    },
                    Instruction::Narrow {
                        destination: 4,
                        source: 3,
                        target: enum_value,
                    },
                    Instruction::Return { source: 2 },
                ],
            ),
        );
        assert_eq!(
            verify(module)
                .expect_err("sources are uninitialized")
                .message,
            "register is not initialized"
        );
    }

    fn call_module(main_effects: u32, callee_effects: u32) -> Module {
        let callback_type = ValueType::Function {
            parameters: vec![ValueType::Int],
            return_type: Box::new(ValueType::Int),
            effects: callee_effects,
        };
        Module {
            constants: vec![Constant::Int(40), Constant::Int(2)],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["fs.read".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Int,
                        ValueType::Int,
                        ValueType::Int,
                        callback_type,
                        ValueType::Int,
                        ValueType::Int,
                    ],
                    return_type: ValueType::Int,
                    effects: main_effects,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Const {
                            destination: 1,
                            constant: 1,
                        },
                        Instruction::DirectCall {
                            destination: 2,
                            function: 1,
                            arguments: vec![0, 1],
                        },
                        Instruction::ClosureNew {
                            destination: 3,
                            function: 2,
                            captures: vec![2],
                        },
                        Instruction::Const {
                            destination: 4,
                            constant: 1,
                        },
                        Instruction::ClosureCall {
                            destination: 5,
                            closure: 3,
                            arguments: vec![4],
                        },
                        Instruction::Return { source: 5 },
                    ],
                },
                Function {
                    name: "add".to_owned(),
                    parameters: vec![0, 1],
                    parameter_names: vec!["_arg0".to_owned(), "_arg1".to_owned()],
                    parameter_default_digests: vec![None, None],
                    captures: vec![],
                    registers: vec![ValueType::Int, ValueType::Int, ValueType::Int],
                    return_type: ValueType::Int,
                    effects: callee_effects,
                    code: vec![
                        Instruction::IntBinary {
                            destination: 2,
                            left: 0,
                            right: 1,
                            operation: NumericBinaryOp::Add,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
                Function {
                    name: "add_captured".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: vec![1],
                    registers: vec![ValueType::Int, ValueType::Int, ValueType::Int],
                    return_type: ValueType::Int,
                    effects: callee_effects,
                    code: vec![
                        Instruction::IntBinary {
                            destination: 2,
                            left: 0,
                            right: 1,
                            operation: NumericBinaryOp::Add,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
            ],
            async_functions: vec![],
            entry: 0,
        }
    }

    #[test]
    fn verifies_exact_direct_and_closure_calls() {
        verify(call_module(1, 1)).expect("typed calls must verify");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn accepts_sourced_sub_agent_closure_call_results() {
        let identity_callback = ValueType::Function {
            parameters: vec![ValueType::SubAgent],
            return_type: Box::new(ValueType::SubAgent),
            effects: 0,
        };
        let direct_result = Module {
            constants: vec![Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: vec![],
                    registers: vec![
                        ValueType::SubAgent,
                        identity_callback,
                        ValueType::SubAgent,
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Unit,
                    effects: 0,
                    code: vec![
                        Instruction::ClosureNew {
                            destination: 1,
                            function: 1,
                            captures: vec![],
                        },
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::ClosureCall {
                            destination: 2,
                            closure: 1,
                            arguments: vec![0],
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Move {
                            destination: 0,
                            source: 2,
                        },
                        Instruction::Const {
                            destination: 3,
                            constant: 0,
                        },
                        Instruction::Return { source: 3 },
                    ],
                },
                Function {
                    name: "identity_handle".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: vec![],
                    registers: vec![ValueType::SubAgent],
                    return_type: ValueType::SubAgent,
                    effects: 0,
                    code: vec![Instruction::Return { source: 0 }],
                },
            ],
            async_functions: vec![0],
            entry: 0,
        };
        verify(direct_result.clone()).expect("SubAgent closure result behavior is accepted");
        verify(direct_result).expect("closure identity result inherits its SubAgent argument");

        let inner_callback = ValueType::Function {
            parameters: vec![ValueType::SubAgent],
            return_type: Box::new(ValueType::SubAgent),
            effects: 0,
        };
        let outer_callback = ValueType::Function {
            parameters: vec![],
            return_type: Box::new(inner_callback.clone()),
            effects: 0,
        };
        let returned_callback = Module {
            constants: vec![Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![outer_callback, inner_callback.clone(), ValueType::Unit],
                    return_type: ValueType::Unit,
                    effects: 0,
                    code: vec![
                        Instruction::ClosureNew {
                            destination: 0,
                            function: 1,
                            captures: vec![],
                        },
                        Instruction::ClosureCall {
                            destination: 1,
                            closure: 0,
                            arguments: vec![],
                        },
                        Instruction::Const {
                            destination: 2,
                            constant: 0,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
                Function {
                    name: "return_callback".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![inner_callback],
                    return_type: ValueType::Function {
                        parameters: vec![ValueType::SubAgent],
                        return_type: Box::new(ValueType::SubAgent),
                        effects: 0,
                    },
                    effects: 0,
                    code: vec![
                        Instruction::ClosureNew {
                            destination: 0,
                            function: 2,
                            captures: vec![],
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
                Function {
                    name: "identity_handle".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: vec![],
                    registers: vec![ValueType::SubAgent],
                    return_type: ValueType::SubAgent,
                    effects: 0,
                    code: vec![Instruction::Return { source: 0 }],
                },
            ],
            async_functions: vec![],
            entry: 0,
        };
        verify(returned_callback.clone())
            .expect("callback-valued closure result behavior is accepted");
        verify(returned_callback)
            .expect("callback-valued results do not themselves store a SubAgent handle");
    }

    #[test]
    fn rejects_duplicate_function_symbol_metadata() {
        let mut module = call_module(1, 1);
        module.functions[1].name = "main".to_owned();
        assert_eq!(
            verify(module).unwrap_err().message,
            "function symbols must be unique"
        );
    }

    #[test]
    fn rejects_noncanonical_function_symbols() {
        for name in [
            "../main.allen::main",
            "/tmp/main.allen::main",
            "main\\evil.allen::main",
            "main.allen::bad\nname",
            "evil\n.allen::helper",
            "C:/secret.allen::helper",
        ] {
            let mut module = call_module(1, 1);
            module.functions[1].name = name.to_owned();
            assert_eq!(
                verify(module).unwrap_err().message,
                "function symbol is not canonical",
                "{name:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_calls_captures_callbacks_and_effects() {
        let mut invalid_function = call_module(1, 1);
        let Instruction::DirectCall { function, .. } = &mut invalid_function.functions[0].code[2]
        else {
            panic!("test instruction must be a direct call");
        };
        *function = 99;
        assert_eq!(
            verify(invalid_function).unwrap_err().message,
            "direct call function ID is out of range"
        );

        let mut invalid_argument = call_module(1, 1);
        invalid_argument.functions[1].registers[0] = ValueType::Bool;
        assert_eq!(
            verify(invalid_argument).unwrap_err().message,
            "call argument"
        );

        let mut invalid_result = call_module(1, 1);
        invalid_result.functions[0].registers[2] = ValueType::Bool;
        assert_eq!(
            verify(invalid_result).unwrap_err().message,
            "direct call result"
        );

        let mut invalid_capture = call_module(1, 1);
        let Instruction::ClosureNew { captures, .. } = &mut invalid_capture.functions[0].code[3]
        else {
            panic!("test instruction must construct a closure");
        };
        captures.clear();
        assert_eq!(
            verify(invalid_capture).unwrap_err().message,
            "call has wrong argument count"
        );

        let mut invalid_callback = call_module(1, 1);
        invalid_callback.functions[0].registers[3] = ValueType::Function {
            parameters: vec![ValueType::Int],
            return_type: Box::new(ValueType::Bool),
            effects: 1,
        };
        assert_eq!(
            verify(invalid_callback).unwrap_err().message,
            "closure construction result"
        );

        assert_eq!(
            verify(call_module(0, 1)).unwrap_err().message,
            "direct call effect set exceeds caller effect set"
        );

        let mut invalid_effect_table = call_module(1, 1);
        invalid_effect_table.effect_sets[1] = vec!["FS.read".to_owned()];
        assert_eq!(
            verify(invalid_effect_table).unwrap_err().message,
            "effect ID is not canonical"
        );
    }

    fn async_module() -> Module {
        Module {
            constants: vec![Constant::Int(7), Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Int,
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Unit,
                    effects: 1,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 1,
                        },
                        Instruction::Await {
                            destination: 2,
                            source: 1,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Const {
                            destination: 3,
                            constant: 1,
                        },
                        Instruction::Return { source: 3 },
                    ],
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        }
    }

    #[test]
    fn verifies_async_types_targets_scopes_and_affine_consumption() {
        verify(async_module()).unwrap();

        let mut wrong_target = async_module();
        wrong_target.async_functions = vec![0];
        assert_eq!(
            verify(wrong_target).unwrap_err().message,
            "async call target is not declared async"
        );

        let mut double_await = async_module();
        double_await.functions[0].code.insert(
            4,
            Instruction::Await {
                destination: 2,
                source: 1,
            },
        );
        assert_eq!(
            verify(double_await).unwrap_err().message,
            "affine register is not live"
        );

        let mut wrong_scope = async_module();
        let Instruction::Spawn { scope, .. } = &mut wrong_scope.functions[0].code[2] else {
            panic!("test instruction must be spawn");
        };
        *scope = 0;
        assert_eq!(
            verify(wrong_scope).unwrap_err().message,
            "spawn scope does not match the current lexical task scope"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn affine_control_flow_accepts_never_edges_and_common_results() {
        let continuing_or_never = Module {
            constants: vec![
                Constant::Bool(true),
                Constant::Unit,
                Constant::String("stopped".to_owned()),
            ],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![Function {
                name: "main".to_owned(),
                parameters: vec![0],
                parameter_names: vec!["_arg0".to_owned()],
                parameter_default_digests: vec![None],
                captures: vec![],
                registers: vec![
                    ValueType::Future(Box::new(ValueType::Int)),
                    ValueType::Bool,
                    ValueType::Int,
                    ValueType::Unit,
                    ValueType::String,
                ],
                return_type: ValueType::Unit,
                effects: 0,
                code: vec![
                    Instruction::Const {
                        destination: 1,
                        constant: 0,
                    },
                    Instruction::BranchBool {
                        condition: 1,
                        true_target: 2,
                        false_target: 5,
                    },
                    Instruction::Await {
                        destination: 2,
                        source: 0,
                    },
                    Instruction::Const {
                        destination: 3,
                        constant: 1,
                    },
                    Instruction::Return { source: 3 },
                    Instruction::Const {
                        destination: 4,
                        constant: 2,
                    },
                    Instruction::Stop { reason: 4 },
                ],
            }],
            async_functions: vec![0],
            entry: 0,
        };
        verify(continuing_or_never).expect("a terminating edge does not join ownership state");

        let common_result = Module {
            constants: vec![Constant::Bool(true), Constant::Int(7)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Bool,
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Int,
                    ],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::BranchBool {
                            condition: 0,
                            true_target: 2,
                            false_target: 5,
                        },
                        Instruction::AsyncCall {
                            destination: 1,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Move {
                            destination: 3,
                            source: 1,
                        },
                        Instruction::Jump { target: 8 },
                        Instruction::AsyncCall {
                            destination: 2,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Move {
                            destination: 3,
                            source: 2,
                        },
                        Instruction::Jump { target: 8 },
                        Instruction::Await {
                            destination: 4,
                            source: 3,
                        },
                        Instruction::Return { source: 4 },
                    ],
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 1,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        };
        verify(common_result)
            .expect("branch-local affine temporaries must not poison a common result register");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn affine_control_flow_requires_identical_outer_obligations_and_scopes() {
        let task_join_module = |consume_false_branch: bool| Module {
            constants: vec![Constant::Bool(true), Constant::Int(7), Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Bool,
                        ValueType::Int,
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Unit,
                    effects: 1,
                    code: vec![
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 0,
                        },
                        Instruction::Const {
                            destination: 2,
                            constant: 0,
                        },
                        Instruction::BranchBool {
                            condition: 2,
                            true_target: 4,
                            false_target: 6,
                        },
                        Instruction::Await {
                            destination: 3,
                            source: 1,
                        },
                        Instruction::Jump { target: 8 },
                        if consume_false_branch {
                            Instruction::Await {
                                destination: 3,
                                source: 1,
                            }
                        } else {
                            Instruction::Const {
                                destination: 4,
                                constant: 2,
                            }
                        },
                        Instruction::Jump { target: 8 },
                        Instruction::Const {
                            destination: 4,
                            constant: 2,
                        },
                        Instruction::Return { source: 4 },
                    ],
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 1,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        };

        verify(task_join_module(true)).expect("both branches consume the outer task");
        let error = verify(task_join_module(false))
            .expect_err("one branch cannot retain an outer task across the join");
        assert_eq!(error.instruction, Some(8));
        assert_eq!(
            error.message,
            "control-flow join has inconsistent affine ownership or task scopes"
        );

        let scope_mismatch = Module {
            constants: vec![Constant::Bool(true), Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![Function {
                name: "main".to_owned(),
                parameters: vec![],
                parameter_names: vec![],
                parameter_default_digests: vec![],
                captures: vec![],
                registers: vec![ValueType::Bool, ValueType::Unit],
                return_type: ValueType::Unit,
                effects: 0,
                code: vec![
                    Instruction::TaskScopeEnter { scope: 1 },
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::BranchBool {
                        condition: 0,
                        true_target: 3,
                        false_target: 5,
                    },
                    Instruction::TaskScopeExit { scope: 1 },
                    Instruction::Jump { target: 6 },
                    Instruction::Jump { target: 6 },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::Return { source: 1 },
                ],
            }],
            async_functions: vec![0],
            entry: 0,
        };
        let error = verify(scope_mismatch).expect_err("scope stacks must agree at a join");
        assert_eq!(error.instruction, Some(6));
        assert_eq!(
            error.message,
            "control-flow join has inconsistent affine ownership or task scopes"
        );
    }

    #[test]
    fn dead_branch_normalization_preserves_must_consume_obligations() {
        let module = Module {
            constants: vec![
                Constant::Bool(true),
                Constant::String("message".to_owned()),
                Constant::Unit,
            ],
            enum_types: vec![],
            effect_sets: vec![vec!["agent.message".to_owned()]],
            functions: vec![Function {
                name: "main".to_owned(),
                parameters: vec![],
                parameter_names: vec![],
                parameter_default_digests: vec![],
                captures: vec![],
                registers: vec![
                    ValueType::Bool,
                    ValueType::String,
                    ValueType::Future(Box::new(
                        effect_result_type(EffectOperation::AgentMessage, None).unwrap(),
                    )),
                    ValueType::Unit,
                ],
                return_type: ValueType::Unit,
                effects: 0,
                code: vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::Const {
                        destination: 3,
                        constant: 2,
                    },
                    Instruction::BranchBool {
                        condition: 0,
                        true_target: 4,
                        false_target: 6,
                    },
                    Instruction::EffectCall {
                        destination: 2,
                        operation: EffectOperation::AgentMessage,
                        arguments: vec![1],
                    },
                    Instruction::Jump { target: 7 },
                    Instruction::Jump { target: 7 },
                    Instruction::Return { source: 3 },
                ],
            }],
            async_functions: vec![],
            entry: 0,
        };

        let error = verify(module)
            .expect_err("an issued external future cannot disappear through a control-flow join");
        assert_eq!(error.instruction, Some(7));
        assert_eq!(
            error.message,
            "control-flow join has inconsistent affine ownership or task scopes"
        );
    }

    #[test]
    fn task_snapshot_observes_a_live_task_without_consuming_it() {
        let mut module = async_module();
        module
            .effect_sets
            .insert(1, vec!["debug.inspect".to_owned(), "task.spawn".to_owned()]);
        module.functions[0].registers.push(task_snapshot_type());
        module.functions[0].code.insert(
            3,
            Instruction::TaskSnapshot {
                destination: 4,
                source: 1,
            },
        );
        verify(module).expect("snapshot must leave its Task source live for await");

        let mut wrong_source = async_module();
        wrong_source
            .effect_sets
            .insert(1, vec!["debug.inspect".to_owned(), "task.spawn".to_owned()]);
        wrong_source.functions[0]
            .registers
            .push(task_snapshot_type());
        wrong_source.functions[0].code.insert(
            3,
            Instruction::TaskSnapshot {
                destination: 4,
                source: 0,
            },
        );
        assert_eq!(
            verify(wrong_source).unwrap_err().message,
            "task snapshot source must be Task"
        );

        let mut wrong_destination = async_module();
        wrong_destination
            .effect_sets
            .insert(1, vec!["debug.inspect".to_owned(), "task.spawn".to_owned()]);
        wrong_destination.functions[0].code.insert(
            3,
            Instruction::TaskSnapshot {
                destination: 2,
                source: 1,
            },
        );
        assert_eq!(
            verify(wrong_destination).unwrap_err().message,
            "task snapshot destination"
        );

        let mut missing_effect = async_module();
        missing_effect.functions[0]
            .registers
            .push(task_snapshot_type());
        missing_effect.functions[0].code.insert(
            3,
            Instruction::TaskSnapshot {
                destination: 4,
                source: 1,
            },
        );
        assert_eq!(
            verify(missing_effect).unwrap_err().message,
            "task snapshot requires the debug.inspect effect"
        );
    }

    #[test]
    fn rejects_live_task_loss_and_scope_escape() {
        let mut live_root = async_module();
        live_root.functions[0].code = vec![
            Instruction::AsyncCall {
                destination: 0,
                function: 1,
                arguments: vec![],
            },
            Instruction::Spawn {
                destination: 1,
                future: 0,
                scope: 0,
            },
            Instruction::Const {
                destination: 3,
                constant: 1,
            },
            Instruction::Return { source: 3 },
        ];
        assert_eq!(
            verify(live_root).unwrap_err().message,
            "return would discard a live affine obligation"
        );

        let mut scoped_return = async_module();
        scoped_return.functions.push(Function {
            name: "scoped".to_owned(),
            parameters: vec![],
            parameter_names: vec![],
            parameter_default_digests: vec![],
            captures: vec![],
            registers: vec![
                ValueType::Future(Box::new(ValueType::Int)),
                ValueType::Task(Box::new(ValueType::Int)),
            ],
            return_type: ValueType::Task(Box::new(ValueType::Int)),
            effects: 1,
            code: vec![
                Instruction::TaskScopeEnter { scope: 1 },
                Instruction::AsyncCall {
                    destination: 0,
                    function: 1,
                    arguments: vec![],
                },
                Instruction::Spawn {
                    destination: 1,
                    future: 0,
                    scope: 1,
                },
                Instruction::Return { source: 1 },
            ],
        });
        scoped_return.async_functions.push(2);
        assert_eq!(
            verify(scoped_return).unwrap_err().message,
            "return cannot leave an explicit task scope"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn propagates_hidden_affine_obligations_through_future_producers() {
        let mut captured_task = async_module();
        captured_task.functions.push(Function {
            name: "consume_task".to_owned(),
            parameters: vec![0],
            parameter_names: vec!["_arg0".to_owned()],
            parameter_default_digests: vec![None],
            captures: vec![],
            registers: vec![ValueType::Task(Box::new(ValueType::Int)), ValueType::Int],
            return_type: ValueType::Int,
            effects: 0,
            code: vec![
                Instruction::Const {
                    destination: 1,
                    constant: 0,
                },
                Instruction::Return { source: 1 },
            ],
        });
        captured_task.async_functions.push(2);
        captured_task.functions[0].registers[2] = ValueType::Future(Box::new(ValueType::Int));
        captured_task.functions[0].code = vec![
            Instruction::AsyncCall {
                destination: 0,
                function: 1,
                arguments: vec![],
            },
            Instruction::Spawn {
                destination: 1,
                future: 0,
                scope: 0,
            },
            Instruction::AsyncCall {
                destination: 2,
                function: 2,
                arguments: vec![1],
            },
            Instruction::Const {
                destination: 3,
                constant: 1,
            },
            Instruction::Return { source: 3 },
        ];
        assert_eq!(
            verify(captured_task).unwrap_err().message,
            "return would discard a live affine obligation"
        );

        let mut future_parameter = async_module();
        future_parameter.functions.push(Function {
            name: "future_parameter".to_owned(),
            parameters: vec![0],
            parameter_names: vec!["_arg0".to_owned()],
            parameter_default_digests: vec![None],
            captures: vec![],
            registers: vec![ValueType::Future(Box::new(ValueType::Int)), ValueType::Unit],
            return_type: ValueType::Unit,
            effects: 0,
            code: vec![
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::Return { source: 1 },
            ],
        });
        assert_eq!(
            verify(future_parameter).unwrap_err().message,
            "return would discard a live affine obligation"
        );

        let mut awaited_future = async_module();
        awaited_future.functions[1].registers =
            vec![ValueType::Future(Box::new(ValueType::Int)), ValueType::Int];
        awaited_future.functions[1].return_type = ValueType::Future(Box::new(ValueType::Int));
        awaited_future.functions[1].code = vec![
            Instruction::AsyncCall {
                destination: 0,
                function: 0,
                arguments: vec![],
            },
            Instruction::Return { source: 0 },
        ];
        awaited_future.functions[0].registers = vec![
            ValueType::Future(Box::new(ValueType::Future(Box::new(ValueType::Int)))),
            ValueType::Future(Box::new(ValueType::Int)),
            ValueType::Unit,
        ];
        awaited_future.functions[0].code = vec![
            Instruction::AsyncCall {
                destination: 0,
                function: 1,
                arguments: vec![],
            },
            Instruction::Await {
                destination: 1,
                source: 0,
            },
            Instruction::Const {
                destination: 2,
                constant: 1,
            },
            Instruction::Return { source: 2 },
        ];
        assert_eq!(
            verify(awaited_future).unwrap_err().message,
            "return would discard a live affine obligation"
        );
    }

    #[test]
    fn rejects_nested_task_result_at_implicit_scope_join() {
        let mut module = async_module();
        module.functions[1]
            .registers
            .push(ValueType::Future(Box::new(ValueType::Int)));
        module.functions[1].return_type = ValueType::Future(Box::new(ValueType::Int));
        module.functions[1].code = vec![
            Instruction::AsyncCall {
                destination: 1,
                function: 1,
                arguments: vec![],
            },
            Instruction::Return { source: 1 },
        ];
        module.functions[0].registers[0] =
            ValueType::Future(Box::new(ValueType::Future(Box::new(ValueType::Int))));
        module.functions[0].registers[1] =
            ValueType::Task(Box::new(ValueType::Future(Box::new(ValueType::Int))));
        module.functions[0].code.remove(3);
        assert_eq!(
            verify(module).unwrap_err().message,
            "nested affine result must be awaited before scope exit"
        );
    }

    #[test]
    fn filesystem_opcodes_require_their_matching_declared_effect() {
        let future = ValueType::Future(Box::new(ValueType::Result(
            Box::new(ValueType::String),
            Box::new(file_error_type()),
        )));
        let function = Function {
            name: "main".to_owned(),
            parameters: vec![],
            parameter_names: vec![],
            parameter_default_digests: vec![],
            captures: vec![],
            registers: vec![
                ValueType::Workspace,
                ValueType::String,
                future.clone(),
                match future {
                    ValueType::Future(result) => *result,
                    _ => unreachable!("test future type is a Future"),
                },
                ValueType::Unit,
            ],
            return_type: ValueType::Unit,
            effects: 0,
            code: vec![
                Instruction::WorkspaceGet { destination: 0 },
                Instruction::Const {
                    destination: 1,
                    constant: 0,
                },
                Instruction::EffectCall {
                    destination: 2,
                    operation: FsOperation::ReadText,
                    arguments: vec![0, 1],
                },
                Instruction::Await {
                    destination: 3,
                    source: 2,
                },
                Instruction::Const {
                    destination: 4,
                    constant: 1,
                },
                Instruction::Return { source: 4 },
            ],
        };
        let mut module = Module {
            constants: vec![Constant::String("notes.txt".to_owned()), Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![function],
            async_functions: vec![0],
            entry: 0,
        };
        assert_eq!(
            verify(module.clone()).unwrap_err().message,
            "filesystem operation requires its matching effect"
        );
        module.effect_sets = vec![vec!["fs.read".to_owned()]];
        verify(module).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn effect_operations_have_exact_signatures_and_effects() {
        let unit = Constant::Unit;
        let make_module = |operation: EffectOperation,
                           mut registers: Vec<ValueType>,
                           argument: Register,
                           constants: Vec<Constant>,
                           setup: Vec<Instruction>,
                           effect: &str| {
            let result = effect_result_type(operation, None).unwrap();
            let future = ValueType::Future(Box::new(result));
            let future_register = u16::try_from(registers.len()).unwrap();
            registers.push(future.clone());
            let result_register = u16::try_from(registers.len()).unwrap();
            registers.push(match future {
                ValueType::Future(result) => *result,
                _ => unreachable!(),
            });
            let unit_register = u16::try_from(registers.len()).unwrap();
            registers.push(ValueType::Unit);
            let mut code = setup;
            code.extend([
                Instruction::EffectCall {
                    destination: future_register,
                    operation,
                    arguments: vec![argument],
                },
                Instruction::Await {
                    destination: result_register,
                    source: future_register,
                },
                Instruction::Const {
                    destination: unit_register,
                    constant: u32::try_from(constants.len()).unwrap(),
                },
                Instruction::Return {
                    source: unit_register,
                },
            ]);
            let mut constants = constants;
            constants.push(unit.clone());
            Module {
                constants,
                enum_types: vec![],
                effect_sets: vec![vec![effect.to_owned()]],
                functions: vec![Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    parameter_names: vec![],
                    parameter_default_digests: vec![],
                    captures: vec![],
                    registers,
                    return_type: ValueType::Unit,
                    effects: 0,
                    code,
                }],
                async_functions: vec![0],
                entry: 0,
            }
        };

        let http = make_module(
            EffectOperation::HttpGet,
            vec![ValueType::String],
            0,
            vec![Constant::String("https://example.test/data".to_owned())],
            vec![Instruction::Const {
                destination: 0,
                constant: 0,
            }],
            "net.http_get",
        );
        verify(http).unwrap();

        let file_request = external_file_request_type();
        let file = make_module(
            EffectOperation::PermissionRequestFile,
            vec![
                ValueType::ExternalFsAccess,
                ValueType::String,
                ValueType::String,
                file_request.clone(),
            ],
            3,
            vec![
                Constant::ExternalFsAccess(ExternalFsAccess::Read),
                Constant::String("/outside/report.txt".to_owned()),
                Constant::String("Read the report.".to_owned()),
            ],
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::Const {
                    destination: 2,
                    constant: 2,
                },
                Instruction::RecordNew {
                    destination: 3,
                    fields: vec![(0, 0), (1, 1), (2, 2)],
                },
            ],
            "permission.request_external_fs",
        );
        verify(file).unwrap();

        let directory_request = external_directory_request_type();
        let directory = make_module(
            EffectOperation::PermissionRequestDirectory,
            vec![
                ValueType::ExternalFsAccess,
                ValueType::String,
                ValueType::String,
                ValueType::Bool,
                directory_request,
            ],
            4,
            vec![
                Constant::ExternalFsAccess(ExternalFsAccess::ReadWrite),
                Constant::String("/outside/reports".to_owned()),
                Constant::String("Update the reports.".to_owned()),
                Constant::Bool(false),
            ],
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::Const {
                    destination: 2,
                    constant: 2,
                },
                Instruction::Const {
                    destination: 3,
                    constant: 3,
                },
                Instruction::RecordNew {
                    destination: 4,
                    fields: vec![(0, 0), (1, 1), (2, 2), (3, 3)],
                },
            ],
            "permission.request_external_fs",
        );
        verify(directory).unwrap();
    }

    #[test]
    fn permission_workspace_result_remains_opaque_and_shape_restricted() {
        let permission_result =
            ValueType::Result(Box::new(ValueType::Workspace), Box::new(file_error_type()));
        let mut invalid_shape = Module {
            constants: vec![Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![Function {
                name: "main".to_owned(),
                parameters: vec![],
                parameter_names: vec![],
                parameter_default_digests: vec![],
                captures: vec![],
                registers: vec![ValueType::Result(
                    Box::new(ValueType::Workspace),
                    Box::new(ValueType::String),
                )],
                return_type: ValueType::Unit,
                effects: 0,
                code: vec![],
            }],
            async_functions: vec![],
            entry: 0,
        };
        assert_eq!(
            verify(invalid_shape.clone()).unwrap_err().message,
            "Future and Task cannot be stored in aggregates"
        );

        invalid_shape.functions[0].registers = vec![permission_result, ValueType::Unknown];
        invalid_shape.functions[0].code = vec![Instruction::ToUnknown {
            destination: 1,
            source: 0,
        }];
        invalid_shape.functions[0].parameters = vec![0];
        invalid_shape.functions[0].parameter_names = vec!["_arg0".into()];
        invalid_shape.functions[0].parameter_default_digests = vec![None];
        assert_eq!(
            verify(invalid_shape).unwrap_err().message,
            "to_unknown source cannot be Function, Future, Task, or Never"
        );
    }
}
