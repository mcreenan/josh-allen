use allen_bytecode::{
    Artifact, ArtifactMetadata, DecodeLimits, EntryContract, Function, Instruction,
    ListLiteralItem, ManifestContract, MapLiteralItem, Module, StrictSchema, ValueType,
    compute_entry_contract_digest, compute_tool_contract_digest, decode, decode_and_verify, encode,
    verify,
};

fn function(registers: Vec<ValueType>, return_type: ValueType, code: Vec<Instruction>) -> Function {
    Function {
        name: "main".to_owned(),
        parameters: Vec::new(),
        parameter_names: Vec::new(),
        parameter_default_digests: Vec::new(),
        captures: Vec::new(),
        registers,
        return_type,
        effects: 0,
        code,
    }
}

fn module(function: Function) -> Module {
    Module {
        constants: Vec::new(),
        enum_types: Vec::new(),
        effect_sets: vec![Vec::new()],
        functions: vec![function],
        async_functions: Vec::new(),
        entry: 0,
    }
}

fn artifact() -> Artifact {
    let list = ValueType::List(Box::new(ValueType::Int));
    let schema = StrictSchema {
        value_type: list.clone(),
    };
    Artifact {
        metadata: ArtifactMetadata {
            bytecode_version: allen_bytecode::BYTECODE_VERSION,
            ..ArtifactMetadata::default()
        },
        module: Module {
            constants: Vec::new(),
            enum_types: Vec::new(),
            effect_sets: vec![Vec::new()],
            functions: vec![{
                let mut function = function(
                    vec![list.clone(), list.clone()],
                    list,
                    vec![
                        Instruction::ListLiteralBuild {
                            destination: 1,
                            items: vec![ListLiteralItem::Spread(0)],
                        },
                        Instruction::Return { source: 1 },
                    ],
                );
                function.name = "pkg/x74657374/x302e312e30/x737263/x6d61696e.allen::main".into();
                function.parameters = vec![0];
                function.parameter_names = vec!["value".into()];
                function.parameter_default_digests = vec![None];
                function
            }],
            async_functions: Vec::new(),
            entry: 0,
        },
        debug: None,
        schemas: vec![schema.clone()],
        entries: vec![EntryContract {
            name: "main".into(),
            function: 0,
            input_schema: 0,
            output_schema: 0,
            input_validators: Vec::new(),
            output_validators: Vec::new(),
            input_record_provenance: Vec::new(),
            output_record_provenance: Vec::new(),
            input_contract_digest: compute_entry_contract_digest(&schema, &[], &[]),
            output_contract_digest: compute_entry_contract_digest(&schema, &[], &[]),
        }],
        imports: Vec::new(),
        manifest: Some(ManifestContract {
            package: "test".into(),
            version: "0.1.0".into(),
            language_requirement: "0.1".into(),
            required_capabilities: Vec::new(),
            optional_capabilities: Vec::new(),
            limits: Vec::new(),
            https_origins: Vec::new(),
            exec_commands: Vec::new(),
            exec_environment: Vec::new(),
            required_tools: Vec::new(),
            tool_contract_digest: compute_tool_contract_digest(&[]),
        }),
        templates: Vec::new(),
        record_invariants: Vec::new(),
    }
}

#[test]
fn verifies_literal_build_item_types_and_sources() {
    let list = ValueType::List(Box::new(ValueType::Int));
    let mut list_function = function(
        vec![list.clone(), list.clone()],
        list.clone(),
        vec![
            Instruction::ListLiteralBuild {
                destination: 1,
                items: vec![ListLiteralItem::Spread(0)],
            },
            Instruction::Return { source: 1 },
        ],
    );
    list_function.parameters = vec![0];
    list_function.parameter_names = vec!["_arg0".to_owned()];
    list_function.parameter_default_digests = vec![None];
    let verified = verify(module(list_function));
    assert!(verified.is_ok(), "{verified:?}");

    let map = ValueType::Map(Box::new(ValueType::Int), Box::new(ValueType::Bool));
    let mut map_function = function(
        vec![map.clone(), map.clone()],
        map,
        vec![
            Instruction::MapLiteralBuild {
                destination: 1,
                items: vec![MapLiteralItem::Spread(0)],
            },
            Instruction::Return { source: 1 },
        ],
    );
    map_function.parameters = vec![0];
    map_function.parameter_names = vec!["_arg0".to_owned()];
    map_function.parameter_default_digests = vec![None];
    let verified = verify(module(map_function));
    assert!(verified.is_ok(), "{verified:?}");
}

#[test]
fn rejects_literal_build_spread_type_mismatches() {
    let list_int = ValueType::List(Box::new(ValueType::Int));
    let list_bool = ValueType::List(Box::new(ValueType::Bool));
    let error = verify(module(function(
        vec![list_bool, list_int.clone()],
        list_int,
        vec![
            Instruction::ListLiteralBuild {
                destination: 1,
                items: vec![ListLiteralItem::Spread(0)],
            },
            Instruction::Return { source: 1 },
        ],
    )))
    .expect_err("mismatched list spread must fail verification");
    assert_eq!(error.message, "list literal spread element");

    let map_int = ValueType::Map(Box::new(ValueType::Int), Box::new(ValueType::Int));
    let map_bool = ValueType::Map(Box::new(ValueType::Int), Box::new(ValueType::Bool));
    let error = verify(module(function(
        vec![map_bool, map_int.clone()],
        map_int,
        vec![
            Instruction::MapLiteralBuild {
                destination: 1,
                items: vec![MapLiteralItem::Spread(0)],
            },
            Instruction::Return { source: 1 },
        ],
    )))
    .expect_err("mismatched map spread must fail verification");
    assert_eq!(error.message, "map literal spread value");
}

#[test]
fn literal_build_and_default_metadata_round_trip_in_artifacts() {
    let mut malformed = artifact();
    malformed.module.functions[0]
        .parameter_default_digests
        .clear();
    let malformed_bytes = encode(&malformed).expect("malformed metadata still encodes");
    let error = decode_and_verify(&malformed_bytes, &DecodeLimits::default())
        .expect_err("malformed metadata must be rejected");
    assert!(error.to_string().contains("default digests"));

    let bytes = encode(&artifact()).expect("artifact encodes");
    let decoded = decode(&bytes, &DecodeLimits::default()).expect("artifact decodes");
    let function = &decoded.artifact().module.functions[0];
    assert_eq!(function.parameter_default_digests, vec![None]);
    assert_eq!(function.code.len(), 2);
    assert_eq!(encode(decoded.artifact()).expect("re-encode"), bytes);
    decode_and_verify(&bytes, &DecodeLimits::default()).expect("artifact verifies");
}

#[test]
fn sequence_fold_moves_an_affine_initial_accumulator() {
    let sequence = ValueType::Sequence(Box::new(ValueType::Int));
    let callback = ValueType::Function {
        parameters: vec![sequence.clone(), ValueType::Int],
        return_type: Box::new(sequence.clone()),
        effects: 0,
    };
    let list = ValueType::List(Box::new(ValueType::Int));
    let mut raw_function = function(
        vec![
            sequence.clone(),
            sequence.clone(),
            callback,
            sequence.clone(),
            list,
        ],
        ValueType::List(Box::new(ValueType::Int)),
        vec![
            Instruction::SequenceFold {
                destination: 3,
                sequence: 0,
                initial: 1,
                callback: 2,
            },
            Instruction::SequenceToList {
                destination: 4,
                sequence: 1,
            },
            Instruction::Return { source: 4 },
        ],
    );
    raw_function.parameters = vec![0, 1, 2];
    raw_function.parameter_names = vec!["source".into(), "initial".into(), "callback".into()];
    raw_function.parameter_default_digests = vec![None, None, None];
    let error = verify(module(raw_function)).expect_err("fold must move its affine initial value");
    assert_eq!(error.message, "affine register is not live");

    let sequence = ValueType::Sequence(Box::new(ValueType::Int));
    let callback = ValueType::Function {
        parameters: vec![sequence.clone(), ValueType::Int],
        return_type: Box::new(sequence.clone()),
        effects: 0,
    };
    let result_list = ValueType::List(Box::new(ValueType::Int));
    let mut alias_function = function(
        vec![sequence.clone(), callback, sequence, result_list],
        ValueType::List(Box::new(ValueType::Int)),
        vec![
            Instruction::SequenceFold {
                destination: 2,
                sequence: 0,
                initial: 0,
                callback: 1,
            },
            Instruction::ListNew {
                destination: 3,
                elements: Vec::new(),
            },
            Instruction::Return { source: 3 },
        ],
    );
    alias_function.parameters = vec![0, 1];
    alias_function.parameter_names = vec!["source".into(), "callback".into()];
    alias_function.parameter_default_digests = vec![None, None];
    let error =
        verify(module(alias_function)).expect_err("one affine value cannot be folded twice");
    assert!(
        error.message.contains("both source and accumulator"),
        "{error:?}"
    );
}

#[test]
fn to_unknown_rejects_sequence_sources_in_raw_modules_and_artifacts() {
    let sequence = ValueType::Sequence(Box::new(ValueType::Int));
    let unknown = ValueType::Unknown;
    let mut raw_function = function(
        vec![sequence, unknown],
        ValueType::Unknown,
        vec![
            Instruction::ToUnknown {
                destination: 1,
                source: 0,
            },
            Instruction::Return { source: 1 },
        ],
    );
    raw_function.parameters = vec![0];
    raw_function.parameter_names = vec!["sequence".into()];
    raw_function.parameter_default_digests = vec![None];
    let raw = module(raw_function);
    let error = verify(raw).expect_err("raw bytecode must reject affine to_unknown");
    assert!(error.message.contains("to_unknown source"), "{error:?}");

    let list = ValueType::List(Box::new(ValueType::Int));
    let mut artifact_function = function(
        vec![
            list.clone(),
            ValueType::Sequence(Box::new(ValueType::Int)),
            ValueType::Unknown,
            list.clone(),
        ],
        list,
        vec![
            Instruction::SequenceFromList {
                destination: 1,
                values: 0,
            },
            Instruction::ToUnknown {
                destination: 2,
                source: 1,
            },
            Instruction::ListNew {
                destination: 3,
                elements: Vec::new(),
            },
            Instruction::Return { source: 3 },
        ],
    );
    artifact_function.parameters = vec![0];
    artifact_function.parameter_names = vec!["value".into()];
    artifact_function.parameter_default_digests = vec![None];
    let mut artifact = artifact();
    artifact_function.name = artifact.module.functions[0].name.clone();
    artifact.module.functions[0] = artifact_function;
    let bytes = encode(&artifact).expect("malformed artifact still encodes");
    let error = decode_and_verify(&bytes, &DecodeLimits::default())
        .expect_err("artifact verification must reject affine to_unknown");
    assert!(error.to_string().contains("to_unknown source"), "{error}");
}
